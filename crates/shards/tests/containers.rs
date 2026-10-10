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
    Run, TempDir, bridge, cannot_run_vms, fixed_port, guest_init, kernel, registry, run_shards_env, served,
    sha256_digest, shards, shards_vm, test_index,
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
    // Kept in the home, which a failing test shows (common::TempDir).
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stderr = std::fs::File::create(home.join(format!("run-{n}.stderr"))).unwrap();
    let mut child = common::command()
        .args(run_args(image, options, command))
        .env("SHARDS_HOME", home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
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

/// More published sockets than one message carries (shards_ipc::MAX_FDS, 9): five
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
        assert_eq!(got, format!("from {}\nhello", bridge().gateway()), "port {port}");
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
fn stats_measures_each_microvm_from_its_guest() {
    let Some((home, image)) = home("containers-stats") else {
        return;
    };
    let mut spinner = start(&home, &image, &["--name", "spinner"], &["spin"]);
    let mut sleeper = start(&home, &image, &["--name", "sleeper"], &["sleep"]);
    let stats = shards_in(&home, &["stats", "--no-stream"]);
    assert_eq!(stats.status, Some(0), "{stats}");
    let mut lines = stats.stdout.lines();
    assert_eq!(
        lines.next(),
        Some("CONTAINER ID   NAME      CPU %     MEM USAGE / LIMIT   MEM %     NET I/O   BLOCK I/O   PIDS"),
        "{stats}"
    );
    // Each row: its name, its share of a CPU, and its memory against the microVM's.
    let row = |name: &str| -> (f64, String) {
        let line = stats
            .stdout
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some(name))
            .unwrap();
        let fields: Vec<&str> = line.split_whitespace().collect();
        let cpu = fields[2].trim_end_matches('%').parse().unwrap();
        // Against the guest's memory, no limit given; its interfaces' and devices'
        // bytes, and its one process.
        assert_eq!(fields[4], "/", "{line}");
        assert!(fields[5].ends_with("MiB"), "{line}");
        assert_eq!(fields.last(), Some(&"1"), "{line}");
        (cpu, fields[3].to_string())
    };
    let (spinning, held) = row("spinner");
    let (sleeping, _) = row("sleeper");
    // A workload spinning one vCPU uses what the host gives that vCPU, which on a busy
    // host is less than a CPU (34% on a shared CI runner, 2026-10-07); one asleep, almost
    // nothing. Measured from each guest, the two stand apart by an order of magnitude.
    assert!(spinning > 10.0 * sleeping.max(1.0) && sleeping < 10.0, "{stats}");
    // The workload's own memory, not its VM's: a binary size.
    assert!(
        ["B", "KiB", "MiB"]
            .iter()
            .any(|u| held.ends_with(u) && held.len() > u.len()),
        "{stats}"
    );
    let named = shards_in(&home, &["stats", "--no-stream", "sleeper"]);
    assert_eq!(named.stdout.lines().count(), 2, "{named}");
    let missing = shards_in(&home, &["stats", "--no-stream", "nobody"]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (Some(1), "Error response from daemon: No such container: nobody\n"),
        "{missing}"
    );
    for name in ["spinner", "sleeper"] {
        assert_eq!(shards_in(&home, &["rm", "-f", name]).status, Some(0));
    }
    exit(&mut spinner);
    exit(&mut sleeper);
}

#[test]
fn pause_freezes_a_microvm_until_unpause_or_stop() {
    let Some((home, image)) = home("containers-pause") else {
        return;
    };
    let mut spinner = start(&home, &image, &["--name", "frozen"], &["spin"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    // Its share of a CPU, as `stats` measures it over a second.
    let cpu = || -> f64 {
        let stats = shards(&["stats", "--no-stream", "frozen"]);
        let line = stats.stdout.lines().nth(1).unwrap().to_string();
        line.split_whitespace()
            .nth(2)
            .unwrap()
            .trim_end_matches('%')
            .parse()
            .unwrap()
    };
    let paused = shards(&["pause", "frozen"]);
    assert_eq!(
        (paused.status, paused.stdout.as_str()),
        (Some(0), "frozen\n"),
        "{paused}"
    );
    let again = shards(&["pause", "frozen"]);
    assert_eq!(again.status, Some(1));
    assert!(again.stderr.ends_with("is already paused\n"), "{again}");
    assert!(cpu() < 5.0, "a frozen microVM spun");
    let ps = shards(&["ps"]);
    assert!(ps.stdout.contains(" (Paused) "), "{ps}");
    let inspected = shards(&["container", "inspect", "frozen"]);
    assert!(inspected.stdout.contains(r#""Status": "paused""#), "{inspected}");
    let rm = shards(&["rm", "frozen"]);
    assert!(
        rm.stderr
            .ends_with("container is paused and must be unpaused first\n"),
        "{rm}"
    );
    let unpaused = shards(&["unpause", "frozen"]);
    assert_eq!(
        (unpaused.status, unpaused.stdout.as_str()),
        (Some(0), "frozen\n"),
        "{unpaused}"
    );
    // Running again: past the bound a frozen one stays under. Not a share of the host,
    // which the tests beside it spinning microVMs of their own leave it no half of.
    let resumed = cpu();
    assert!(
        resumed > 5.0,
        "a resumed microVM did not run: {resumed}% of a CPU"
    );
    let not = shards(&["unpause", "frozen"]);
    assert!(not.stderr.ends_with("is not paused\n"), "{not}");
    // A stop lets a frozen microVM go on to hear its signal, as dockerd's does.
    assert_eq!(shards(&["pause", "frozen"]).status, Some(0));
    let stopped = shards(&["stop", "-t", "5", "frozen"]);
    assert_eq!(
        (stopped.status, stopped.stdout.as_str()),
        (Some(0), "frozen\n"),
        "{stopped}"
    );
    assert_eq!(exit(&mut spinner), Some(128 + 15));
}

#[test]
fn top_lists_a_microvms_processes_as_docker_top_does() {
    let Some((home, image)) = home("containers-top") else {
        return;
    };
    let mut sleeper = start(&home, &image, &["--name", "listed"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let top = shards(&["top", "listed"]);
    assert_eq!(top.status, Some(0), "{top}");
    let mut lines = top.stdout.lines();
    // docker/cli's tabwriter: each title at least 20 wide (top.go).
    assert_eq!(
        lines.next(),
        Some(
            "UID                 PID                 PPID                C                   STIME               TTY                 TIME                CMD"
        ),
        "{top}"
    );
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split_whitespace().collect()).collect();
    // The workload, as the image's user named in its own /etc/passwd, with its
    // argument; nothing of init's.
    assert!(
        rows.iter()
            .any(|r| r.first() == Some(&"app") && r.last() == Some(&"sleep")),
        "{top}"
    );
    assert!(!top.stdout.contains("shards-init"), "{top}");
    let chosen = shards(&["top", "listed", "-o", "pid,args"]);
    assert!(
        chosen.stdout.starts_with("PID                 COMMAND\n"),
        "{chosen}"
    );
    let refused = shards(&["top", "listed", "-o", "user=PID"]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: specifying \"user=PID\" is not allowed\n"
        ),
        "{refused}"
    );
    assert_eq!(shards(&["pause", "listed"]).status, Some(0));
    let paused = shards(&["top", "listed"]);
    assert!(paused.stderr.contains("is paused"), "{paused}");
    assert_eq!(shards(&["unpause", "listed"]).status, Some(0));
    assert_eq!(shards(&["stop", "listed"]).status, Some(0));
    exit(&mut sleeper);
    let ended = shards(&["top", "listed"]);
    assert!(ended.stderr.ends_with("is not running\n"), "{ended}");
}

#[test]
fn diff_shows_what_a_microvm_changed_as_docker_diff_does() {
    let Some((home, image)) = home("containers-diff") else {
        return;
    };
    let mut sleeper = start(&home, &image, &["--name", "changed"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    // Nothing yet: what init makes of every container is not the container's change.
    let fresh = shards(&["diff", "changed"]);
    assert_eq!((fresh.status, fresh.stdout.as_str()), (Some(0), ""), "{fresh}");
    let changed = shards(&[
        "exec",
        "-u",
        "0",
        "changed",
        "/bin/testguest",
        "fs",
        "mkdir:/new",
        "write:/new/f=x",
        "write:/etc/passwd=root:x:0:0::/:/bin/sh",
        "rm:/etc/group",
        "chmod:700:/home",
        "write:/etc/hosts=elsewhere",
    ]);
    assert_eq!(changed.status, Some(0), "{changed}");
    let diff = shards(&["diff", "changed"]);
    // A parent of a change is changed; /etc/hosts is init's, as Docker's init layer's.
    assert_eq!(
        (diff.status, diff.stdout.as_str()),
        (
            Some(0),
            "C /etc\nD /etc/group\nC /etc/passwd\nC /home\nA /new\nA /new/f\n"
        ),
        "{diff}"
    );
    assert_eq!(shards(&["stop", "changed"]).status, Some(0));
    exit(&mut sleeper);
    // Stopped, its changes are still there to show (visit.rs).
    let ended = shards(&["diff", "changed"]);
    assert_eq!(ended.status, Some(0), "{ended}");
}

#[test]
fn events_tell_a_microvms_life_as_docker_events_does() {
    let Some((home, image)) = home("containers-events") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    // A listener, which hears renames as they happen.
    let mut live = common::command()
        .args(["events", "--since", &since, "--filter", "event=rename"])
        .env("SHARDS_HOME", &*home)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut sleeper = start(&home, &image, &["--name", "lived"], &["sleep"]);
    for step in [
        &["pause", "lived"][..],
        &["unpause", "lived"],
        &["rename", "lived", "lived2"],
        &["stop", "lived2"],
    ] {
        assert_eq!(shards(step).status, Some(0), "{step:?}");
    }
    exit(&mut sleeper);
    assert_eq!(shards(&["rm", "lived2"]).status, Some(0));
    // Past: only what had happened is shown, and the command ends.
    let until = format!(
        "{:.9}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    );
    let past = shards(&[
        "events",
        "--since",
        &since,
        "--until",
        &until,
        "--filter",
        "type=container",
    ]);
    assert_eq!(past.status, Some(0), "{past}");
    // Each line: time, type, action, ID, attributes.
    let actions: Vec<&str> = past.stdout.lines().filter_map(|l| l.split(' ').nth(2)).collect();
    assert_eq!(
        actions,
        [
            "create", "start", "pause", "unpause", "rename", "kill", "die", "stop", "destroy"
        ],
        "{past}"
    );
    assert!(past.stdout.contains("rename"), "{past}");
    assert!(past.stdout.contains("name=lived2, oldName=/lived)"), "{past}");
    assert!(
        past.stdout.contains("kill ") && past.stdout.contains("signal=15"),
        "{past}"
    );
    assert!(past.stdout.contains("exitCode=143"), "{past}");
    let json = shards(&[
        "events",
        "--since",
        &since,
        "--until",
        &until,
        "--format",
        "json",
        "--filter",
        "event=destroy",
    ]);
    assert!(
        json.stdout
            .starts_with("{\"Type\":\"container\",\"Action\":\"destroy\",\"Actor\":{\"ID\":"),
        "{json}"
    );
    let templated = shards(&[
        "events",
        "--since",
        &since,
        "--until",
        &until,
        "--filter",
        "event=rename",
        "--format",
        "{{.Action}} {{.Actor.Attributes.name}} {{index .Actor.Attributes \"oldName\" | upper}}",
    ]);
    assert_eq!(templated.stdout, "rename lived2 /LIVED\n", "{templated}");
    let bad = shards(&["events", "--format", "{{.Nope"]);
    assert_eq!(bad.status, Some(64), "{bad}");
    assert!(
        bad.stderr.starts_with("Error parsing format: template: :1: "),
        "{bad}"
    );
    let mut line = String::new();
    BufReader::new(live.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(
        line.contains(" container rename ") && line.contains("name=lived2"),
        "{line}"
    );
    let _ = live.kill();
    let _ = live.wait();
}

#[test]
fn export_writes_a_microvms_files_as_a_tar_archive() {
    let Some((home, image)) = home("containers-export") else {
        return;
    };
    let mut sleeper = start(&home, &image, &["--name", "exported"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = shards(&[
        "exec",
        "-u",
        "0",
        "exported",
        "/bin/testguest",
        "fs",
        "write:/made=by the microVM",
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let out = home.join("exported.tar");
    let exported = shards(&["export", "-o", out.to_str().unwrap(), "exported"]);
    assert_eq!(
        (exported.status, exported.stdout.as_str()),
        (Some(0), ""),
        "{exported}"
    );
    let tar = std::fs::read(&out).unwrap();
    // Its own files, what it wrote, and nothing of what is mounted over its root.
    let has = |name: &[u8]| tar.windows(name.len()).any(|w| w == name);
    assert!(has(b"bin/testguest\0") && has(b"made\0") && has(b"by the microVM"));
    let listed = Command::new("tar").arg("tf").arg(&out).output().unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    let names: Vec<&str> = listed.lines().collect();
    assert!(names.contains(&"proc/") && names.contains(&"dev/"), "{listed}");
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("proc/") && *n != "proc/" || n.starts_with("dev/") && *n != "dev/"),
        "{listed}"
    );
    assert_eq!(tar.len() % 512, 0);
    assert_eq!(shards(&["stop", "exported"]).status, Some(0));
    exit(&mut sleeper);
    // Stopped, its files are still there to export (visit.rs).
    let ended = shards(&["export", "-o", out.to_str().unwrap(), "exported"]);
    assert_eq!(ended.status, Some(0), "{ended}");
    let again = std::fs::read(&out).unwrap();
    assert!(
        again
            .windows(b"by the microVM".len())
            .any(|w| w == b"by the microVM")
    );
}

#[test]
fn cp_copies_files_into_and_out_of_a_microvm_as_docker_cp_does() {
    let Some((home, image)) = home("containers-cp") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let mut sleeper = start(&home, &image, &["--name", "copier", "-u", "0"], &["sleep"]);
    let src = home.join("cp-src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), "one").unwrap();
    std::fs::write(src.join("sub/b.txt"), "two").unwrap();
    // In: a directory under a new name, then out again, as it was.
    let into = shards(&["cp", src.to_str().unwrap(), "copier:/in"]);
    assert_eq!(
        (into.status, into.stdout.as_str(), into.stderr.as_str()),
        (Some(0), "", ""),
        "{into}"
    );
    let back = home.join("cp-back");
    let out = shards(&["container", "cp", "copier:/in", back.to_str().unwrap()]);
    assert_eq!(out.status, Some(0), "{out}");
    assert_eq!(std::fs::read_to_string(back.join("a.txt")).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(back.join("sub/b.txt")).unwrap(), "two");
    // Into an existing directory, said: the file's size, and the archive's that carried it.
    let said = shards(&[
        "cp",
        "-q=false",
        src.join("a.txt").to_str().unwrap(),
        "copier:/in/sub",
    ]);
    assert_eq!(
        (said.status, said.stderr.as_str()),
        (
            Some(0),
            "Successfully copied 3B (transferred 2.05kB) to copier:/in/sub\n"
        ),
        "{said}"
    );
    let file = shards(&[
        "cp",
        "copier:/in/sub/a.txt",
        home.join("a-again").to_str().unwrap(),
    ]);
    assert_eq!(file.status, Some(0), "{file}");
    assert_eq!(std::fs::read_to_string(home.join("a-again")).unwrap(), "one");
    // A tar archive to stdout.
    let streamed = shards(&["cp", "copier:/in/a.txt", "-"]);
    assert_eq!(streamed.status, Some(0), "{streamed}");
    assert!(
        streamed.stdout.contains("a.txt\0") && streamed.stdout.contains("one"),
        "{streamed}"
    );
    // In Docker's words.
    let missing = shards(&["cp", "copier:/nope", home.join("x").to_str().unwrap()]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: Could not find the file /nope in container copier\n"
        ),
        "{missing}"
    );
    let across = shards(&["cp", "copier:/in", "copier:/out"]);
    assert_eq!(
        across.stderr, "copying between containers is not supported\n",
        "{across}"
    );
    let neither = shards(&["cp", "a", "b"]);
    assert_eq!(
        neither.stderr, "must specify at least one container source\n",
        "{neither}"
    );
    let no_dir = shards(&["cp", "copier:/in", home.join("none/there").to_str().unwrap()]);
    assert_eq!(no_dir.status, Some(1), "{no_dir}");
    assert!(
        no_dir.stderr.starts_with("invalid output path: directory "),
        "{no_dir}"
    );
    assert_eq!(shards(&["stop", "copier"]).status, Some(0));
    exit(&mut sleeper);
    // Stopped, its files are still there to copy (visit.rs).
    let ended = shards(&["cp", "copier:/in", home.join("cp-stopped").to_str().unwrap()]);
    assert_eq!(ended.status, Some(0), "{ended}");
    assert_eq!(
        std::fs::read_to_string(home.join("cp-stopped/a.txt")).unwrap(),
        "one"
    );
}

#[test]
fn a_stopped_microvms_files_are_read_and_written_as_dockerd_does() {
    let Some((home, image)) = home("containers-visit") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = run_in(
        &home,
        &image,
        &["--name", "resting", "-u", "0"],
        &["fs", "write:/made=yes", "rm:/etc/group"],
    );
    assert_eq!(made.status, Some(0), "{made}");
    // Its state and exit code; not its age, which the visits below take time from.
    let state = "{{.Names}} {{.State}}";
    let before = shards(&["ps", "-a", "--format", state]);
    assert_eq!(before.stdout, "resting exited\n", "{before}");
    let code = shards(&["inspect", "-f", "{{.State.ExitCode}}", "resting"]);
    assert_eq!(code.stdout, "0\n", "{code}");
    // Its changes, read in a VM over its files, as dockerd reads a stopped container's.
    let diff = shards(&["diff", "resting"]);
    assert_eq!(diff.status, Some(0), "{diff}");
    assert!(
        diff.stdout.contains("A /made\n") && diff.stdout.contains("D /etc/group\n"),
        "{diff}"
    );
    // Copied in, kept; copied out.
    let note = home.join("note");
    std::fs::write(&note, "visited").unwrap();
    let into = shards(&["cp", note.to_str().unwrap(), "resting:/note"]);
    assert_eq!((into.status, into.stderr.as_str()), (Some(0), ""), "{into}");
    let out = home.join("made-out");
    let copied = shards(&["cp", "resting:/made", out.to_str().unwrap()]);
    assert_eq!(copied.status, Some(0), "{copied}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "yes");
    assert!(shards(&["diff", "resting"]).stdout.contains("A /note\n"));
    let tar = home.join("resting.tar");
    let exported = shards(&["export", "-o", tar.to_str().unwrap(), "resting"]);
    assert_eq!(exported.status, Some(0), "{exported}");
    let listed = Command::new("tar").arg("tf").arg(&tar).output().unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.lines().any(|n| n == "made") && listed.lines().any(|n| n == "note"),
        "{listed}"
    );
    assert!(!listed.lines().any(|n| n == "etc/group"), "{listed}");
    // Still stopped, as it was: the visits started and ended nothing.
    let after = shards(&["ps", "-a", "--format", state]);
    assert_eq!(after.stdout, before.stdout);
    // Until a second on: `0s` is this second's start, as the client sends whole seconds.
    let until = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 1)
    .to_string();
    let events = shards(&[
        "events",
        "--since",
        "1",
        "--until",
        &until,
        "--format",
        "{{.Action}}",
    ]);
    let actions: Vec<&str> = events.stdout.lines().collect();
    assert_eq!(actions.iter().filter(|a| **a == "start").count(), 1, "{events}");
    assert_eq!(actions.iter().filter(|a| **a == "die").count(), 1, "{events}");
    assert!(
        actions.contains(&"extract-to-dir") && actions.contains(&"export"),
        "{events}"
    );
    // Started again, over what was copied in.
    let started = shards(&["start", "-a", "resting"]);
    assert!(
        started.stderr.contains("rm:/etc/group: No such file"),
        "{started}"
    );
    let again = shards(&["cp", "resting:/note", home.join("note-again").to_str().unwrap()]);
    assert_eq!(again.status, Some(0), "{again}");
    assert_eq!(
        std::fs::read_to_string(home.join("note-again")).unwrap(),
        "visited"
    );
    // top, as dockerd's, reads only a running one.
    let top = shards(&["top", "resting"]);
    assert_eq!(top.status, Some(1), "{top}");
}

#[test]
fn start_runs_a_stopped_microvm_again_over_its_own_files() {
    let Some((home, image)) = home("containers-start") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    // Each run adds a line to a file of its own, and removes one of the image's.
    let first = run_in(
        &home,
        &image,
        &["--name", "again", "-u", "0"],
        &["fs", "write:/etc/variant=x", "rm:/etc/group", "mkdir:/made"],
    );
    assert_eq!(first.status, Some(0), "{first}");
    let started = shards(&["start", "again"]);
    assert_eq!(
        (started.status, started.stdout.as_str()),
        (Some(0), "again\n"),
        "{started}"
    );
    let waited = shards(&["wait", "again"]);
    // Its later runs find /etc/group gone, as its first left it.
    assert_eq!(waited.stdout, "1\n", "{waited}");
    let attached = shards(&["start", "-a", "again"]);
    assert_eq!(attached.status, Some(1), "{attached}");
    assert!(
        attached.stderr.contains("rm:/etc/group: No such file"),
        "{attached}"
    );
    // Made, not started; then started.
    let made = shards(&["create", "--name", "later", &image, "sleep"]);
    assert_eq!(made.status, Some(0), "{made}");
    let ps = shards(&["ps", "-a", "--format", "{{.Names}} {{.State}}"]);
    assert!(ps.stdout.contains("later created\n"), "{ps}");
    assert_eq!(shards(&["start", "later"]).stdout, "later\n");
    let restarted = shards(&["restart", "-t", "1", "later"]);
    assert_eq!(
        (restarted.status, restarted.stdout.as_str()),
        (Some(0), "later\n"),
        "{restarted}"
    );
    let ps = shards(&["ps", "--format", "{{.Names}} {{.State}}"]);
    assert_eq!(ps.stdout, "later running\n", "{ps}");
    let missing = shards(&["start", "nobody"]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: No such container: nobody\nfailed to start containers: nobody\n"
        ),
        "{missing}"
    );
    assert_eq!(shards(&["rm", "-f", "later"]).status, Some(0));
}

#[test]
fn commit_makes_an_image_of_what_a_microvm_changed() {
    let Some((home, image)) = home("containers-commit") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    // Stopped: its kept layer.
    let made = run_in(
        &home,
        &image,
        &["--name", "changer", "-u", "0"],
        &["fs", "write:/kept=yes", "rm:/etc/group"],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let committed = shards(&["commit", "-m", "a step", "changer", "test/committed:v1"]);
    assert_eq!(committed.status, Some(0), "{committed}");
    assert!(committed.stdout.starts_with("sha256:"), "{committed}");
    let read = run_in(
        &home,
        "test/committed:v1",
        &["--rm", "-u", "0"],
        &["fs", "rm:/kept", "rm:/etc/group"],
    );
    // Its file is there to remove; the image's file it removed is not.
    assert_eq!(read.status, Some(1), "{read}");
    assert!(read.stderr.contains("rm:/etc/group: No such file"), "{read}");
    let history = shards(&["history", "--format", "{{.Comment}}", "test/committed:v1"]);
    assert_eq!(history.stdout.lines().next(), Some("a step"), "{history}");
    // Running: paused for it, then going on.
    let mut sleeper = start(&home, &image, &["--name", "live", "-u", "0"], &["sleep"]);
    let wrote = shards(&["exec", "-u", "0", "live", "/bin/testguest", "fs", "write:/live=1"]);
    assert_eq!(wrote.status, Some(0), "{wrote}");
    let live = shards(&["commit", "live", "test/committed:live"]);
    assert_eq!(live.status, Some(0), "{live}");
    let ps = shards(&["ps", "--format", "{{.Names}} {{.State}}"]);
    assert_eq!(ps.stdout, "live running\n", "{ps}");
    let read = run_in(
        &home,
        "test/committed:live",
        &["--rm", "-u", "0"],
        &["fs", "rm:/live"],
    );
    assert_eq!(read.status, Some(0), "{read}");
    // Its configuration changed as a Dockerfile's instructions would.
    let changed = shards(&[
        "commit",
        "-c",
        "ENV STAGE=committed",
        "-c",
        "WORKDIR /srv",
        "live",
        "test/committed:changed",
    ]);
    assert_eq!(changed.status, Some(0), "{changed}");
    let inspected = shards(&["image", "inspect", "test/committed:changed"]);
    assert!(inspected.stdout.contains("\"STAGE=committed\""), "{inspected}");
    assert!(
        inspected.stdout.contains("\"WorkingDir\": \"/srv\""),
        "{inspected}"
    );
    let refused = shards(&["commit", "-c", "FROM x", "live"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    let both = shards(&["commit", "--pause", "--no-pause", "live"]);
    assert_eq!(
        (both.status, both.stderr.as_str()),
        (
            Some(1),
            "conflicting options: --no-pause and --pause cannot be used together\n"
        ),
        "{both}"
    );
    assert_eq!(shards(&["rm", "-f", "live"]).status, Some(0));
    exit(&mut sleeper);
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
fn ps_filters_microvms_as_dockerd_filters_containers() {
    let Some((home, image)) = home("containers-ps-filter") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    assert_eq!(
        run_in(&home, &image, &["--name", "zero"], &["exit", "0"]).status,
        Some(0)
    );
    assert_eq!(
        run_in(&home, &image, &["--name", "three"], &["exit", "3"]).status,
        Some(3)
    );
    let mut sleeper = start(&home, &image, &["--name", "up"], &["sleep"]);
    let names = |filters: &[&str]| {
        let mut args = vec!["ps", "--format", "{{.Names}}"];
        args.extend_from_slice(filters);
        let listed = shards(&args);
        assert_eq!(listed.status, Some(0), "{listed}");
        listed.stdout
    };
    // A status lists stopped ones without -a; an exit code does not.
    assert_eq!(names(&["-f", "status=exited"]), "three\nzero\n");
    assert_eq!(names(&["-f", "exited=3"]), "");
    assert_eq!(names(&["-a", "-f", "exited=3"]), "three\n");
    // Names as regular expressions, IDs by a prefix.
    assert_eq!(names(&["-a", "-f", "name=^[tu]"]), "up\nthree\n");
    let id = shards(&["ps", "-aq", "--no-trunc", "-f", "name=zero"]).stdout;
    assert_eq!(
        names(&["-a", "-f", &format!("id={}", id.get(..12).unwrap())]),
        "zero\n"
    );
    // Before and since, by name or ID.
    assert_eq!(names(&["-a", "-f", "before=three"]), "zero\n");
    assert_eq!(
        names(&["-a", "-f", &format!("since={}", id.trim())]),
        "up\nthree\n"
    );
    assert_eq!(
        names(&["-a", "-f", "health=none", "-f", "status=running"]),
        "up\n"
    );
    // Its image, and images made from it.
    let committed = shards(&["commit", "zero", "test/child:1"]);
    assert_eq!(committed.status, Some(0), "{committed}");
    assert_eq!(
        run_in(&home, "test/child:1", &["--name", "kid"], &["exit", "0"]).status,
        Some(0)
    );
    assert_eq!(
        names(&["-a", "-f", &format!("ancestor={image}")]),
        "kid\nup\nthree\nzero\n"
    );
    assert_eq!(names(&["-a", "-f", "ancestor=test/child:1"]), "kid\n");
    assert_eq!(names(&["-a", "-f", "ancestor=nothing:here"]), "");
    // Refused in dockerd's words.
    for (filter, said) in [
        ("bogus=1", "invalid filter 'bogus'"),
        (
            "status=gone",
            "invalid filter 'status=gone': invalid value for state (gone): must be one of created, running, paused, restarting, removing, exited, dead",
        ),
        (
            "exited=x",
            "invalid filter 'exited=x': strconv.Atoi: parsing \"x\": invalid syntax",
        ),
        ("before=nobody", "No such container: nobody"),
        ("publish=1:2", "filter for 'publish' should not contain ':': 1:2"),
    ] {
        let refused = shards(&["ps", "-f", filter]);
        assert_eq!(
            (refused.status, refused.stderr.as_str()),
            (Some(1), format!("Error response from daemon: {said}\n").as_str()),
            "{refused}"
        );
    }
    assert_eq!(shards(&["rm", "-f", "up"]).status, Some(0));
    exit(&mut sleeper);
}

#[test]
fn images_and_prunes_filter_as_dockerd_does() {
    let Some((home, image)) = home("containers-image-filter") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    assert_eq!(
        run_in(&home, &image, &["--name", "maker"], &["exit", "0"]).status,
        Some(0)
    );
    let committed = shards(&["commit", "-c", "LABEL tier=web", "maker", "test/labelled:1"]);
    assert_eq!(committed.status, Some(0), "{committed}");
    let listed = |filters: &[&str]| {
        let mut args = vec!["images", "--format", "{{.Repository}}:{{.Tag}}"];
        args.extend_from_slice(filters);
        let out = shards(&args);
        assert_eq!(out.status, Some(0), "{out}");
        out.stdout
    };
    assert_eq!(listed(&["-f", "label=tier=web"]), "test/labelled:1\n");
    assert_eq!(listed(&["-f", "label=tier"]), "test/labelled:1\n");
    assert!(!listed(&["-f", "label!=tier"]).contains("labelled"));
    assert_eq!(listed(&["-f", "reference=test/*"]), "test/labelled:1\n");
    assert_eq!(listed(&["test/labelled"]), "test/labelled:1\n");
    assert_eq!(
        listed(&["-f", "reference=docker.io/test/labelled"]),
        "test/labelled:1\n"
    );
    // The test image says no time it was made: dockerd then filters nothing by it.
    assert!(listed(&["-f", &format!("since={image}")]).starts_with("test/labelled:1\n"));
    assert!(!listed(&["-f", "before=test/labelled:1"]).contains("labelled"));
    assert_eq!(listed(&["-f", "until=1h"]), "");
    assert_eq!(listed(&["-f", "dangling=true"]), "");
    for (filter, said) in [
        ("bogus=1", "invalid filter 'bogus'"),
        ("before=nothing:here", "No such image: nothing:here"),
        ("dangling=maybe", "invalid filter 'dangling=[maybe]'"),
        (
            "until=x",
            "invalid value for 'until' filter: failed to parse value as time or duration: invalid seconds \"x\": invalid syntax",
        ),
    ] {
        let refused = shards(&["images", "-f", filter]);
        assert_eq!(
            (refused.status, refused.stderr.as_str()),
            (Some(1), format!("Error response from daemon: {said}\n").as_str()),
            "{refused}"
        );
    }
    // Prunes, filtered: nothing older than an hour, nothing labelled.
    let young = shards(&["container", "prune", "-f", "--filter", "until=1h"]);
    assert_eq!(young.stdout, "Total reclaimed space: 0B\n", "{young}");
    let unlabelled = shards(&["container", "prune", "-f", "--filter", "label=x"]);
    assert_eq!(unlabelled.stdout, "Total reclaimed space: 0B\n", "{unlabelled}");
    let twice = shards(&[
        "container",
        "prune",
        "-f",
        "--filter",
        "until=1h",
        "--filter",
        "until=2h",
    ]);
    assert_eq!(
        twice.stderr, "Error response from daemon: more than one until filter specified\n",
        "{twice}"
    );
    let wrong = shards(&["system", "prune", "-f", "--filter", "dangling=true"]);
    assert_eq!(
        wrong.stderr, "Error response from daemon: invalid filter 'dangling'\n",
        "{wrong}"
    );
    let stopped = shards(&["container", "prune", "-f"]);
    assert!(stopped.stdout.starts_with("Deleted Containers:\n"), "{stopped}");
    // Every unused image the label names, and no other.
    let pruned = shards(&["image", "prune", "-af", "--filter", "label=tier=web"]);
    assert_eq!(pruned.status, Some(0), "{pruned}");
    assert!(pruned.stdout.contains("untagged: test/labelled:1\n"), "{pruned}");
    assert_eq!(listed(&["-f", "reference=test/*"]), "");
    assert!(
        listed(&[]).lines().count() >= 1,
        "the image it was made from stays"
    );
}

#[test]
fn inspect_answers_as_docker_inspect_does() {
    let Some((home, image)) = home("containers-inspect-any") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let mut sleeper = start(&home, &image, &["--name", "probe", "-e", "A=1"], &["sleep"]);
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.State.Status}} {{.State.Running}} {{index .Config.Env 0}} {{.Name}}",
        "probe",
    ]);
    assert_eq!(
        (shown.status, shown.stdout.as_str()),
        (Some(0), "running true A=1 /probe\n"),
        "{shown}"
    );
    // Not a Go field of the response, but a key of its JSON: read there, as the CLI does.
    let id = shards(&["inspect", "-f", "{{.Id}}", "probe"]);
    assert_eq!(id.stdout.trim().len(), 64, "{id}");
    let typed = shards(&["inspect", "-f", "{{.ID}}", "probe"]);
    assert_eq!(typed.stdout, id.stdout);
    let health = shards(&["container", "inspect", "-f", "{{.State.Health.Status}}", "probe"]);
    assert_eq!(health.status, Some(1), "{health}");
    assert!(
        health.stderr.contains(r#"map has no entry for key "Health""#),
        "{health}"
    );
    let bad = shards(&["inspect", "-f", "{{.Nope", "probe"]);
    assert_eq!(bad.status, Some(64), "{bad}");
    assert!(bad.stderr.starts_with("template parsing error: "), "{bad}");
    let json = shards(&["container", "inspect", "--format", "json", "probe"]);
    let docs: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    // Attached, as `start` runs it; on the bridge, where every guest is .2.
    assert_eq!(docs[0]["Config"]["AttachStdout"], true, "{json}");
    assert!(
        docs[0]["NetworkSettings"]["Networks"]["bridge"]["IPAddress"]
            .as_str()
            .unwrap()
            .ends_with(".2")
    );
    assert_eq!(docs[0]["HostConfig"]["NetworkMode"], "bridge", "{json}");
    assert!(docs[0]["State"]["Pid"].as_u64().unwrap() > 0, "{json}");
    // Images too, typed: a time prints as Go's Time.String.
    let at = shards(&[
        "inspect",
        "--type",
        "image",
        "-f",
        "{{.Metadata.LastTagTime}}",
        &image,
    ]);
    assert_eq!(at.status, Some(0), "{at}");
    assert!(at.stdout.trim_end().ends_with(" +0000 UTC"), "{at}");
    // What is not there, in the CLI's and dockerd's words.
    let none = shards(&["inspect", "nosuch"]);
    assert_eq!(
        (none.status, none.stdout.as_str(), none.stderr.as_str()),
        (Some(1), "[]\n", "error: no such object: nosuch\n"),
        "{none}"
    );
    let volume = shards(&["inspect", "--type", "volume", "v"]);
    assert_eq!(
        volume.stderr, "Error response from daemon: get v: no such volume\n",
        "{volume}"
    );
    let empty = shards(&["inspect", "--type", "", "x"]);
    assert!(
        empty
            .stderr
            .starts_with("type is empty: must be one of \"config\""),
        "{empty}"
    );
    assert_eq!(shards(&["rm", "-f", "probe"]).status, Some(0));
    exit(&mut sleeper);
}

#[test]
fn run_labels_names_and_resolves_as_docker_run_does() {
    let Some((home, image)) = home("containers-run-flags") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let env_file = home.join("env.list");
    std::fs::write(&env_file, "# comment\nFROM_FILE=1\n\nSHARDS_TEST_UNSET_VAR\n").unwrap();
    // A bare name in a label file is dropped, as kvfile.Parse drops it without a lookup.
    let label_file = home.join("labels.list");
    std::fs::write(&label_file, "tier=file\nfiled\n").unwrap();
    let options = [
        "--name",
        "flagged",
        "--label",
        "tier=cli",
        "-l",
        "bare",
        "--label-file",
        label_file.to_str().unwrap(),
        "--env-file",
        env_file.to_str().unwrap(),
        "--expose",
        "7000-7001/udp",
        "--add-host",
        "db.internal:10.9.8.7",
        "--add-host",
        "gw=host-gateway",
        "--dns",
        "9.9.9.9",
        "--dns-search",
        "example.org",
        "--dns-option",
        "ndots:2",
        "--hostname",
        "box",
        "--domainname",
        "example.org",
    ];
    let ran = run_in(
        &home,
        &image,
        &options,
        &["stat", "/etc/hosts", "/etc/resolv.conf"],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let text: Vec<&str> = ran.stdout.lines().filter_map(|l| l.strip_prefix("= ")).collect();
    let [hosts, resolv] = text[..] else {
        panic!("{ran}");
    };
    assert!(hosts.contains("10.9.8.7\tdb.internal\\n"), "{hosts}");
    assert!(hosts.contains("\tgw\\n"), "{hosts}");
    assert!(hosts.contains("\tbox.example.org box\\n"), "{hosts}");
    assert_eq!(
        resolv, "nameserver 9.9.9.9\\nsearch example.org\\noptions ndots:2\\n",
        "{ran}"
    );
    let shown = shards(&[
        "inspect",
        "-f",
        "{{json .Config.Labels}} {{json .Config.ExposedPorts}} {{.Config.Domainname}} {{.HostConfig.Dns}} {{.HostConfig.DnsSearch}} {{.HostConfig.DnsOptions}} {{.HostConfig.ExtraHosts}}",
        "flagged",
    ]);
    assert_eq!(
        (shown.status, shown.stdout.as_str()),
        (
            Some(0),
            r#"{"bare":"","tier":"cli"} {"7000/udp":{},"7001/udp":{}} example.org [9.9.9.9] [example.org] [ndots:2] [db.internal:10.9.8.7 gw:host-gateway]
"#
        ),
        "{shown}"
    );
    // The env file's lines, and a bare name from the client's environment, which it has not.
    let report = run_in(
        &home,
        &image,
        &["--env-file", env_file.to_str().unwrap()],
        &["report"],
    );
    assert!(report.stdout.contains("env FROM_FILE=1\n"), "{report}");
    assert!(!report.stdout.contains("SHARDS_TEST_UNSET_VAR"), "{report}");
    let listed = shards(&[
        "ps",
        "-a",
        "--filter",
        "label=tier=cli",
        "--format",
        "{{.Names}} {{.Label \"tier\"}}",
    ]);
    assert_eq!(listed.stdout, "flagged cli\n", "{listed}");
    let none = shards(&["ps", "-a", "--filter", "label=tier=file", "-q"]);
    assert_eq!(none.stdout, "", "{none}");
    // Docker's own refusals.
    let bad = shards(&["run", "--pull", "never", "--add-host", "nohost", &image]);
    assert_eq!(bad.status, Some(125), "{bad}");
    assert!(
        bad.stderr.contains(
            "invalid argument \"nohost\" for \"--add-host\" flag: bad format for add-host: \"nohost\""
        ),
        "{bad}"
    );
    let pruned = shards(&["container", "prune", "-f", "--filter", "label!=tier"]);
    assert_eq!(pruned.status, Some(0), "{pruned}");
    let left = shards(&["ps", "-a", "--format", "{{.Names}}", "--filter", "name=flagged"]);
    assert_eq!(left.stdout, "flagged\n", "{left}");
}

/// `--cidfile`, `-q`, `--platform` and `--sig-proxy`, as docker run and create take them:
/// the ID written once the microVM is made, and the file left alone or removed as
/// docker/cli's cidFile does; a pull said nothing of; the platform read as containerd
/// reads it, and one this host's microVMs cannot run refused before anything is pulled;
/// signals kept from the command.
#[test]
fn run_writes_cidfiles_pulls_quietly_and_keeps_signals_as_docker_run_does() {
    let Some((home, image)) = home("containers-cidfile") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let cid = home.join("run.cid");
    let cid_arg = cid.to_str().unwrap();
    let ran = run_in(
        &home,
        &image,
        &["--cidfile", cid_arg, "--name", "cided"],
        &["exit", "0"],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let id = shards(&[
        "inspect",
        "-f",
        "{{.Id}} {{.HostConfig.ContainerIDFile}}",
        "cided",
    ]);
    assert_eq!(
        id.stdout,
        format!("{} {cid_arg}\n", std::fs::read_to_string(&cid).unwrap()),
        "{id}"
    );
    // One there already is the other container's: left as it is.
    let again = run_in(&home, &image, &["--cidfile", cid_arg], &["exit", "0"]);
    assert_eq!(
        (again.status, again.stderr.as_str()),
        (
            Some(125),
            format!(
                "shards: container ID file found, make sure the other container isn't running or delete {cid_arg}\n\nRun 'shards run --help' for more information\n"
            )
            .as_str()
        ),
        "{again}"
    );
    assert_eq!(std::fs::read_to_string(&cid).unwrap().len(), 64);
    // A directory that is not there: toStatusError's 127.
    let lost = home.join("nowhere/x.cid");
    let missing = run_in(
        &home,
        &image,
        &["--cidfile", lost.to_str().unwrap()],
        &["exit", "0"],
    );
    assert_eq!(missing.status, Some(127), "{missing}");
    assert!(
        missing.stderr.starts_with(&format!(
            "shards: failed to create the container ID file: open {}: no such file or directory\n",
            lost.display()
        )),
        "{missing}"
    );
    // No container, no ID: the file goes.
    let unmade = home.join("unmade.cid");
    let none = shards(&[
        "run",
        "--pull",
        "never",
        "--cidfile",
        unmade.to_str().unwrap(),
        "nosuch/image",
    ]);
    assert_eq!(none.status, Some(125), "{none}");
    assert!(!unmade.exists(), "{none}");
    // `create` writes it too, and says the same ID.
    let made = home.join("create.cid");
    let created = shards(&[
        "create",
        "--pull",
        "never",
        "--cidfile",
        made.to_str().unwrap(),
        &image,
    ]);
    assert_eq!(created.status, Some(0), "{created}");
    assert_eq!(
        created.stdout,
        format!("{}\n", std::fs::read_to_string(&made).unwrap())
    );
    let refused = shards(&[
        "create",
        "--pull",
        "never",
        "--cidfile",
        made.to_str().unwrap(),
        &image,
    ]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (
            Some(1),
            format!(
                "container ID file found, make sure the other container isn't running or delete {}\n",
                made.display()
            )
            .as_str()
        ),
        "{refused}"
    );
    // This host's platform, however it is spelled, runs; another is refused, and a
    // specifier containerd cannot read is said as it says it.
    let (ours, other) = if std::env::consts::ARCH == "aarch64" {
        ("linux/arm64/v8", "linux/amd64")
    } else {
        ("linux/amd64", "linux/arm64")
    };
    let native = run_in(&home, &image, &["--platform", ours], &["exit", "0"]);
    assert_eq!(native.status, Some(0), "{native}");
    let foreign = run_in(&home, &image, &["--platform", other], &["exit", "0"]);
    assert_eq!(foreign.status, Some(125), "{foreign}");
    assert!(
        foreign
            .stderr
            .starts_with("shards: this host's microVMs run linux/"),
        "{foreign}"
    );
    assert!(foreign.stderr.contains(&format!(", not {other}\n")), "{foreign}");
    let unread = run_in(&home, &image, &["--platform", "nope"], &["exit", "0"]);
    assert_eq!(unread.status, Some(125), "{unread}");
    assert!(
        unread
            .stderr
            .starts_with("shards: \"nope\": unknown operating system or architecture: invalid argument\n"),
        "{unread}"
    );
    // `-q`: an image not yet here is pulled without a word.
    let (fresh, _) = served();
    let quiet = shards(&["run", "-q", &fresh, "exit", "0"]);
    assert_eq!(quiet.status, Some(0), "{quiet}");
    assert!(
        !quiet.stderr.contains("Unable to find image") && !quiet.stderr.contains("Pulling from"),
        "{quiet}"
    );
    // `--sig-proxy=false`: the client's SIGTERM ends the client, and the command runs on.
    let mut kept = start(
        &home,
        &image,
        &["--sig-proxy=false", "--name", "unproxied"],
        &["sleep"],
    );
    let pid = libc::pid_t::try_from(kept.id()).unwrap();
    // SAFETY: kill(2) of the client this test started, not yet waited for.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert_eq!(exit(&mut kept), None);
    let state = shards(&["inspect", "-f", "{{.State.Status}}", "unproxied"]);
    assert_eq!(state.stdout, "running\n", "{state}");
    let removed = shards(&["rm", "-f", "unproxied"]);
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// `run`'s memory and CPU flags as docker run takes them: checked as dockerd checks them,
/// its warnings said, the microVM sized to hold them, and the workload's cgroup limited
/// as runc limits it, seen read-only at /sys/fs/cgroup as a container sees its own; a
/// workload past its memory killed, and OOMKilled, as Docker reports it.
#[test]
fn run_limits_resources_as_docker_run_does() {
    let Some((home, image)) = home("containers-resources") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let files = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory.low",
        "/sys/fs/cgroup/memory.swap.max",
        "/sys/fs/cgroup/cpu.max",
        "/sys/fs/cgroup/cpu.weight",
        "/sys/fs/cgroup/pids.max",
        "/sys/fs/cgroup/cpuset.cpus",
        "/proc/self/cgroup",
        "/sys/devices/system/cpu/online",
    ];
    let mut command = vec!["stat"];
    command.extend(files);
    let limited = run_in(
        &home,
        &image,
        &[
            "--name",
            "limited",
            "-m",
            "64m",
            "--memory-reservation",
            "32m",
            "--cpus",
            "1.5",
            "-c",
            "512",
            "--pids-limit",
            "50",
            "--cpuset-cpus",
            "0",
            "--memory-swappiness",
            "50",
        ],
        &command,
    );
    assert_eq!(limited.status, Some(0), "{limited}");
    assert!(
        limited.stderr.starts_with(
            "WARNING: Your kernel does not support memory swappiness capabilities or the cgroup is not mounted. Memory swappiness discarded.\n"
        ),
        "{limited}"
    );
    let read: Vec<&str> = limited
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("= "))
        .collect();
    assert_eq!(
        read,
        [
            "67108864\\n",
            "33554432\\n",
            "67108864\\n",
            "150000 100000\\n",
            "59\\n",
            "50\\n",
            "0\\n",
            "0::/\\n",
            "0-1\\n",
        ],
        "{limited}"
    );
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.HostConfig.MemoryReservation}} {{.HostConfig.NanoCPUs}} {{.HostConfig.CPUShares}} {{.HostConfig.PidsLimit}} {{.HostConfig.CpusetCpus}} {{.HostConfig.MemorySwappiness}}",
        "limited",
    ]);
    assert_eq!(
        shown.stdout, "67108864 134217728 33554432 1500000000 512 50 0 <nil>\n",
        "{shown}"
    );
    // JSON's names fail on the Go types, and are read from the JSON, where a null is no
    // value, as docker/cli's inspector falls back.
    let raw = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.CpuShares}} {{.HostConfig.MemorySwappiness}}",
        "limited",
    ]);
    assert_eq!(raw.stdout, "512 <no value>\n", "{raw}");
    // The microVM holds what the limit allows: what the kernel says may be allocated is
    // at least the limit, across the sizes where its own share steps up (M117).
    for limit in ["700m", "2500m"] {
        let meminfo = run_in(&home, &image, &["-m", limit], &["stat", "/proc/meminfo"]);
        assert_eq!(meminfo.status, Some(0), "{meminfo}");
        let available_kib: u64 = meminfo
            .stdout
            .split("\\n")
            .find_map(|l| l.strip_prefix("MemAvailable:"))
            .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
            .unwrap();
        let limit_kib = limit.trim_end_matches('m').parse::<u64>().unwrap() * 1024;
        assert!(
            available_kib >= limit_kib,
            "{limit}: {available_kib} KiB available"
        );
    }
    // Past its limit, the workload is killed, as the kernel kills it in its cgroup.
    let hog = run_in(&home, &image, &["--name", "hog", "-m", "32m"], &["alloc", "100"]);
    assert_eq!(hog.status, Some(137), "{hog}");
    let state = shards(&["inspect", "-f", "{{.State.OOMKilled}} {{.State.ExitCode}}", "hog"]);
    assert_eq!(state.stdout, "true 137\n", "{state}");
    let within = run_in(&home, &image, &["-m", "64m"], &["alloc", "16"]);
    assert_eq!(
        (within.status, within.stdout.as_str()),
        (Some(0), "allocated 16\n"),
        "{within}"
    );
    // dockerd's refusals: `run` with the CLI's prefix, help and 125; `create` as they are.
    let small = run_in(&home, &image, &["-m", "4m"], &["exit", "0"]);
    assert_eq!(
        (small.status, small.stderr.as_str()),
        (
            Some(125),
            "shards: Error response from daemon: Minimum memory limit allowed is 6MB\n\nRun 'shards run --help' for more information\n"
        ),
        "{small}"
    );
    let made = shards(&["create", "--pull", "never", "-m", "4m", &image]);
    assert_eq!(
        (made.status, made.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: Minimum memory limit allowed is 6MB\n"
        ),
        "{made}"
    );
    let cpus = std::thread::available_parallelism().unwrap().get();
    let many = run_in(&home, &image, &["--cpus", "100000"], &["exit", "0"]);
    assert_eq!(many.status, Some(125), "{many}");
    assert!(
        many.stderr.contains(&format!(
            "range of CPUs is from 0.01 to {cpus}.00, as there are only {cpus} CPUs available"
        )),
        "{many}"
    );
    let swappy = run_in(&home, &image, &["--memory-swappiness", "101"], &["exit", "0"]);
    assert_eq!(
        (swappy.status, swappy.stderr.as_str()),
        (
            Some(125),
            "shards: invalid value: 101. Valid memory swappiness range is 0-100\n\nRun 'shards run --help' for more information\n"
        ),
        "{swappy}"
    );
}

/// `--read-only`, `--tmpfs`, `--shm-size`, `--ulimit` and `--sysctl` as docker run takes
/// them: the workload's root read-only and its tmpfs mounts in its own mount namespace,
/// which an exec joins; its rlimits, which an exec takes too; its sysctls set; inspect
/// as dockerd keeps them; and dockerd's and runc's refusals.
#[test]
fn run_sets_up_mounts_limits_and_sysctls_as_docker_run_does() {
    let Some((home, image)) = home("containers-setup") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let sealed = [
        "--read-only",
        "--tmpfs",
        "/run:size=1m,exec",
        "--shm-size",
        "128m",
        "--ulimit",
        "nofile=1024:2048",
        "--sysctl",
        "net.core.somaxconn=1024",
    ];
    // The workload's own view.
    let rooted = run_in(&home, &image, &sealed, &["fs", "write:/run/y=1", "write:/x=1"]);
    assert_eq!(rooted.status, Some(1), "{rooted}");
    assert!(
        rooted.stderr.contains("write:/x=1: Read-only file system"),
        "{rooted}"
    );
    let mut options = vec!["--name", "sealed"];
    options.extend(sealed);
    let mut held = start(&home, &image, &options, &["sleep"]);
    // An exec's: the same mounts and limits.
    let exec = |args: &[&str]| {
        let mut all = vec!["exec", "sealed", "/bin/testguest"];
        all.extend(args);
        shards(&all)
    };
    let wrote = exec(&["fs", "write:/run/z=1"]);
    assert_eq!(wrote.status, Some(0), "{wrote}");
    let refused = exec(&["fs", "write:/z=1"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    let read = exec(&[
        "stat",
        "/proc/sys/net/core/somaxconn",
        "/proc/self/limits",
        "/proc/self/mounts",
    ]);
    let files: Vec<&str> = read.stdout.lines().filter_map(|l| l.strip_prefix("= ")).collect();
    let [somaxconn, limits, mounts] = files[..] else {
        panic!("{read}");
    };
    assert_eq!(somaxconn, "1024\\n", "{read}");
    assert!(
        limits.split("\\n").any(|l| l.starts_with("Max open files")
            && l.split_whitespace().collect::<Vec<_>>()[3..5] == ["1024", "2048"]),
        "{limits}"
    );
    assert!(mounts.contains("overlay / overlay ro,"), "{mounts}");
    assert!(
        mounts.contains("tmpfs /run tmpfs rw,nosuid,nodev,relatime,size=1024k"),
        "{mounts}"
    );
    assert!(
        mounts.contains("/dev/shm tmpfs rw,nosuid,nodev,noexec,relatime,size=131072k"),
        "{mounts}"
    );
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.ReadonlyRootfs}} {{json .HostConfig.Tmpfs}} {{.HostConfig.ShmSize}} {{json .HostConfig.Ulimits}} {{json .HostConfig.Sysctls}}",
        "sealed",
    ]);
    assert_eq!(
        shown.stdout,
        "true {\"/run\":\"size=1m,exec\"} 134217728 [{\"Name\":\"nofile\",\"Hard\":2048,\"Soft\":1024}] {\"net.core.somaxconn\":\"1024\"}\n",
        "{shown}"
    );
    // checkWritablePath: nothing is copied onto a read-only root.
    let file = home.join("copied.txt");
    std::fs::write(&file, "x").unwrap();
    let copied = shards(&["cp", file.to_str().unwrap(), "sealed:/copied.txt"]);
    assert_eq!(
        (copied.status, copied.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: container rootfs is marked read-only\n"
        ),
        "{copied}"
    );
    assert_eq!(shards(&["kill", "sealed"]).status, Some(0));
    exit(&mut held);
    // dockerd's refusals as it makes the container, then runc's as it starts it.
    let said = |options: &[&str]| run_in(&home, &image, options, &["exit", "0"]);
    let help = "\n\nRun 'shards run --help' for more information\n";
    for (options, status, words) in [
        (
            &["--tmpfs", "/"][..],
            125,
            "invalid specification: destination can't be '/'",
        ),
        (
            &["--tmpfs", "run"][..],
            125,
            "invalid mount path: 'run' mount path must be absolute",
        ),
        (&["--tmpfs", "/x:foo"][..], 125, "invalid tmpfs option \"foo\""),
        (
            &["--tmpfs", "/x:size=abc"][..],
            125,
            "error mounting \"tmpfs\" to rootfs at \"/x\": mount src=tmpfs, dst=/x, flags=MS_NOSUID|MS_NODEV|MS_NOEXEC, data=size=abc: invalid argument",
        ),
        (
            &["--sysctl", "net.core.nope=1"][..],
            127,
            "open sysctl net.core.nope file: open /proc/sys/net/core/nope: no such file or directory",
        ),
        (
            &["--sysctl", "net.core.somaxconn=x"][..],
            125,
            "failed to write sysctl net.core.somaxconn = \"x\": write /proc/sys/net/core/somaxconn: invalid argument",
        ),
    ] {
        let r = said(options);
        let stderr: String = r
            .stderr
            .split_inclusive('\n')
            .filter(|l| !l.starts_with("shards-timing "))
            .collect();
        assert_eq!(
            (r.status, stderr.as_str()),
            (
                Some(status),
                format!("shards: Error response from daemon: {words}{help}").as_str()
            ),
            "{options:?}"
        );
    }
}

/// `--cap-add`, `--cap-drop`, `--group-add`, `--oom-score-adj` and `--privileged` as docker
/// run takes them, and `exec --privileged`: capabilities as moby tweaks them, groups as
/// moby adds them, the OOM score set before the command, an exec taking the workload's;
/// a privileged microVM unmasked, its /sys writable, every capability; dockerd's refusals.
#[test]
fn run_sets_capabilities_groups_and_privileges_as_docker_run_does() {
    let Some((home, image)) = home("containers-privileges") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let tweaked = [
        "--cap-add",
        "net_admin",
        "--cap-drop",
        "chown",
        "--group-add",
        "staff",
        "--group-add",
        "1234",
        "--oom-score-adj",
        "-500",
    ];
    let line = |out: &str, key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ').map(str::to_string))
            .unwrap_or_default()
    };
    let mut options = vec!["-u", "root"];
    options.extend(tweaked);
    let ran = run_in(&home, &image, &options, &["report"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    // Docker's defaults, CHOWN dropped and NET_ADMIN added.
    assert_eq!(line(&ran.stdout, "capeff"), "00000000a80435fa", "{ran}");
    assert_eq!(line(&ran.stdout, "capbnd"), "00000000a80435fa", "{ran}");
    assert_eq!(line(&ran.stdout, "groups"), "0,50,1234", "{ran}");
    // An exec takes the workload's process: its bounding set, groups and OOM score.
    let mut held_options = vec!["--name", "tweaked"];
    held_options.extend(tweaked);
    let mut held = start(&home, &image, &held_options, &["sleep"]);
    let exec = |args: &[&str]| {
        let mut all = vec!["exec"];
        all.extend(args);
        shards(&all)
    };
    let as_app = exec(&["tweaked", "/bin/testguest", "report"]);
    assert_eq!(line(&as_app.stdout, "capeff"), "0000000000000000", "{as_app}");
    assert_eq!(line(&as_app.stdout, "capbnd"), "00000000a80435fa", "{as_app}");
    assert_eq!(line(&as_app.stdout, "groups"), "50,50,1000,1234", "{as_app}");
    let oom = exec(&["tweaked", "/bin/testguest", "stat", "/proc/self/oom_score_adj"]);
    assert!(oom.stdout.contains("= -500\\n"), "{oom}");
    let privileged = exec(&[
        "--privileged",
        "-u",
        "root",
        "tweaked",
        "/bin/testguest",
        "report",
    ]);
    assert_eq!(
        line(&privileged.stdout, "capeff"),
        "000001ffffffffff",
        "{privileged}"
    );
    // `--env-file`: its lines, before `-e`'s, which wins (docker/cli exec.go parseExec).
    let env_file = home.join("exec.env");
    std::fs::write(&env_file, "# a comment\nFROM_EXEC_FILE=1\nBOTH=file\n").unwrap();
    let with_env = exec(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "-e",
        "BOTH=flag",
        "tweaked",
        "/bin/testguest",
        "report",
    ]);
    assert!(with_env.stdout.contains("env FROM_EXEC_FILE=1\n"), "{with_env}");
    assert!(with_env.stdout.contains("env BOTH=flag\n"), "{with_env}");
    assert!(!with_env.stdout.contains("env BOTH=file\n"), "{with_env}");
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.CapAdd}} {{.HostConfig.CapDrop}} {{.HostConfig.GroupAdd}} {{.HostConfig.OomScoreAdj}} {{.HostConfig.Privileged}}",
        "tweaked",
    ]);
    assert_eq!(
        shown.stdout, "[CAP_NET_ADMIN] [CAP_CHOWN] [staff 1234] -500 false\n",
        "{shown}"
    );
    assert_eq!(shards(&["kill", "tweaked"]).status, Some(0));
    exit(&mut held);
    // Privileged: every capability, nothing masked, /sys writable.
    let all = run_in(
        &home,
        &image,
        &["-u", "root", "--privileged"],
        &["stat", "/proc/self/status", "/proc/self/mounts"],
    );
    assert_eq!(all.status, Some(0), "{all}");
    assert!(all.stdout.contains("CapEff:\t000001ffffffffff"), "{all}");
    assert!(all.stdout.contains("sysfs /sys sysfs rw,"), "{all}");
    assert!(!all.stdout.contains(" /proc/kcore "), "{all}");
    assert!(!all.stdout.contains(" /proc/sys proc ro,"), "{all}");
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.SecurityOpt}} {{.HostConfig.MaskedPaths}}",
        "--type",
        "container",
        "tweaked",
    ]);
    assert_eq!(shown.status, Some(0), "{shown}");
    // dockerd's refusals.
    let help = "\n\nRun 'shards run --help' for more information\n";
    for (options, words) in [
        (
            &["--cap-add", "foo"][..],
            "invalid CapAdd: unknown capability: \"CAP_FOO\"",
        ),
        (
            &["--cap-drop", "nope"][..],
            "invalid CapDrop: unknown capability: \"CAP_NOPE\"",
        ),
        (
            &["--oom-score-adj", "1001"][..],
            "Invalid value 1001, range for oom score adj is [-1000, 1000]",
        ),
        (
            &["--group-add", "nope"][..],
            "unable to find group nope: no matching entries in group file",
        ),
    ] {
        let r = run_in(&home, &image, options, &["exit", "0"]);
        let stderr: String = r
            .stderr
            .split_inclusive('\n')
            .filter(|l| !l.starts_with("shards-timing "))
            .collect();
        assert_eq!(
            (r.status, stderr.as_str()),
            (
                Some(125),
                format!("shards: Error response from daemon: {words}{help}").as_str()
            ),
            "{options:?}"
        );
    }
}

#[test]
fn info_and_disk_usage_format_as_docker_does() {
    let Some((home, image)) = home("containers-info-df") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    assert_eq!(
        run_in(&home, &image, &["--name", "kept"], &["exit", "0"]).status,
        Some(0)
    );
    let info = shards(&[
        "info",
        "-f",
        "{{.Containers}} {{.ContainersStopped}} {{.Images}} {{.Driver}} {{.ClientInfo.Context}}",
    ]);
    assert_eq!(
        (info.status, info.stdout.as_str()),
        (Some(0), "1 1 1 erofs default\n"),
        "{info}"
    );
    let json = shards(&["system", "info", "--format", "json"]);
    let doc: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(doc["Swarm"]["LocalNodeState"], "inactive", "{json}");
    assert_eq!(doc["ServerVersion"], doc["ClientInfo"]["Version"], "{json}");
    let bad = shards(&["info", "-f", "{{.Nope"]);
    assert_eq!(bad.status, Some(64), "{bad}");
    // docker/cli's DiskUsageContext: the summary table, a template, and -v's tables.
    let df = shards(&["system", "df"]);
    let lines: Vec<&str> = df.stdout.lines().collect();
    assert!(
        lines[0].starts_with("TYPE ") && lines[0].ends_with("RECLAIMABLE"),
        "{df}"
    );
    assert!(
        lines[1].starts_with("Images ") && lines[2].starts_with("Containers "),
        "{df}"
    );
    assert!(
        lines[3].starts_with("Local Volumes ") && lines[4].starts_with("Build Cache "),
        "{df}"
    );
    let types = shards(&["system", "df", "--format", "{{.Type}}:{{.TotalCount}}"]);
    assert_eq!(
        types.stdout, "Images:1\nContainers:1\nLocal Volumes:0\nBuild Cache:0\n",
        "{types}"
    );
    let verbose = shards(&["system", "df", "-v"]);
    assert!(verbose.stdout.starts_with("Images space usage:\n"), "{verbose}");
    assert!(
        verbose.stdout.contains("\nContainers space usage:\n"),
        "{verbose}"
    );
    assert!(verbose.stdout.contains(" kept\n"), "{verbose}");
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
    // Its stdin, without `-i`, is /dev/null, as Docker's is; with it, a pipe.
    assert!(reported.stdout.contains("stdin null\n"), "{reported}");
    let piped = run_in(&home, &image, &["-i", "--rm"], &["report"]);
    assert!(piped.stdout.contains("stdin pipe\n"), "{piped}");
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
    let follower = common::command()
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

/// This build's `shards` in a directory of its own, with a VM process binary
/// (`SHARDS_VM_BINARY`) that waits for a gate to open before it becomes this build's.
/// Every VM its daemon starts waits there, so a run stays pending, its container created,
/// for as long as the test keeps the gate shut (audit A06).
struct Gated {
    dir: TempDir,
}

impl Gated {
    fn new(name: &str) -> Gated {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new(name);
        for (from, to) in [(shards(), "shards")] {
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
        // Its copy of shards was written a moment ago.
        let mut child = common::spawn(
            Command::new(self.dir.join("shards"))
                .args(args)
                .env("SHARDS_HOME", home)
                .env("SHARDS_LOCAL_STORE", "none")
                .env("SHARDS_KERNEL", kernel())
                .env("SHARDS_INIT", guest_init())
                .env("SHARDS_VM_BINARY", self.dir.join("shards-vm"))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
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
            signal: None,
            stdout: self.out.join().unwrap(),
            stderr: self.err.join().unwrap(),
            elapsed: start.elapsed(),
        }
    }
}

/// The ID of the one container in `home`, once it is there: created, its run pending.
fn created(gated: &Gated, home: &Path, run: &mut Going) -> String {
    let id = std::cell::RefCell::new(String::new());
    let last = std::cell::RefCell::new(String::new());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let listed = gated.shards(home, &["ps", "-a", "-q", "--no-trunc"]);
        *id.borrow_mut() = listed.stdout.clone();
        if !id.borrow().is_empty() {
            break;
        }
        *last.borrow_mut() = listed.to_string();
        if Instant::now() >= deadline {
            // Where the run stands: what the daemon said, and every shards process.
            let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(40).collect();
            let procs = Command::new("ps")
                .args(["-eo", "pid,ppid,stat,etime,args"])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter(|l| l.contains("shards"))
                        .map(|l| format!("{l}\n"))
                        .collect::<String>()
                })
                .unwrap_or_default();
            // The run's client, if it has ended: what it said.
            let client = match run.child.try_wait() {
                Ok(Some(status)) => {
                    // Its output is whole once it has ended.
                    let err = std::mem::replace(&mut run.err, std::thread::spawn(String::new))
                        .join()
                        .unwrap_or_default();
                    format!("ended {status}, saying: {err}")
                }
                Ok(None) => "still going".to_string(),
                Err(e) => format!("unknown: {e}"),
            };
            panic!(
                "the run's container never showed in {TIMEOUT:?}; the last ps: {}\nthe run's client: {client}\ndaemon.log, last first:\n{}\nprocesses:\n{procs}",
                last.borrow(),
                tail.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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
    let mut run = gated.start(&home, &args);
    let id = created(&gated, &home, &mut run);
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
    let mut run = gated.start(&home, &args);
    let id = created(&gated, &home, &mut run);
    let pid: i32 = std::fs::read_to_string(home.join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: kill(2) of the daemon this test's run started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let run = run.finish();
    assert_ne!(run.status, Some(0), "{run}");
    // The next command finds the daemon gone, as one a person types later does.
    common::until_unserved(&home);
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
    let mut run = gated.start(&home, &args);
    let id = created(&gated, &home, &mut run);
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
    let mut command = common::command();
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
    let stopped = without_vms(&home, &["daemon", "stop"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
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
    let stopped = without_vms(&home, &["daemon", "stop"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
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
    let out = common::command()
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
    let stopped = without_vms(&home, &["daemon", "stop"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
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
    assert!(
        files.stdout.contains(&format!("{}\tbox\\n", bridge().guest())),
        "{files}"
    );
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

/// A datagram a run's policy refuses is answered at once with ICMP's "administratively
/// prohibited" (RFC 1812 §5.2.7.1), which a connected socket hears as EHOSTUNREACH,
/// where one dropped left a resolver to wait out its timeouts (review 2.27). The
/// address is TEST-NET-1's (RFC 5737): no answer could come from it anyway.
#[test]
fn a_refused_datagram_is_said_refused_at_once() {
    let Some((home, image)) = home("containers-udp-refused") else {
        return;
    };
    // The home's first run boots its VM and saves its template; the probe's alone is timed.
    let warm = run_in(&home, &image, &["--rm"], &["exit", "0"]);
    assert_eq!(warm.status, Some(0), "{warm}");
    let t0 = std::time::Instant::now();
    let ran = run_in(&home, &image, &["--rm", "-u", "root"], &["udp", "192.0.2.1:53"]);
    assert_eq!(ran.status, Some(1), "{ran}");
    // EHOSTUNREACH, Linux's 113 on every architecture shards runs (asm-generic/errno.h).
    assert!(
        ran.stdout.starts_with("udp error ") && ran.stdout.contains("(os error 113)"),
        "{ran}"
    );
    // The probe waits 5 s for an answer that never comes.
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
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
    let on_bridge = bridge();
    for ip in [on_bridge.guest(), on_bridge.gateway()] {
        assert!(!files.stdout.contains(&ip.to_string()), "{files}");
    }
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
    // An address on the bridge the daemon elected, which only a user-defined network takes.
    let on_bridge = format!(
        "name=bridge,ip={}",
        std::net::Ipv4Addr::from(u32::from(bridge().subnet().0) + 9)
    );
    for (options, said) in [
        (
            &["--network", "host"][..],
            "shards: Error response from daemon: \"--network host\" is not supported by shards yet",
        ),
        (
            &["--network", on_bridge.as_str()][..],
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
        "stdin null\n",
    ] {
        assert!(report.stdout.contains(line), "{line:?}\n{report}");
    }
    let interactive = exec(&["-i", "ex", "/bin/testguest", "report"]);
    assert!(interactive.stdout.contains("stdin pipe\n"), "{interactive}");
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
    let mut cat = common::command()
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
    // -it with a stdin that is no terminal: refused once the container is found, and not
    // with -d; the detach keys are checked before it is looked for (docker/cli exec.go).
    let untty = "cannot attach stdin to a TTY-enabled container because stdin is not a terminal\n";
    refused(&["-it", "ex", "/bin/testguest", "report"], 1, untty);
    refused(
        &["-it", "nope", "/bin/testguest", "report"],
        1,
        "Error response from daemon: No such container: nope\n",
    );
    assert_eq!(exec(&["-dit", "ex", "/bin/testguest", "sleep"]).status, Some(0));
    refused(
        &[
            "-it",
            "--detach-keys",
            "ctrl-P",
            "nope",
            "/bin/testguest",
            "report",
        ],
        1,
        "invalid detach keys (ctrl-P): Unknown character: 'ctrl-P'\n",
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
    // Before the exec is made: before whether the container runs is asked.
    refused(&["-it", "ex", "/bin/testguest", "report"], 1, untty);
}

/// Who holds TCP port `port`, as lsof lists them: each process's ID, command and
/// sockets, for a test to say whose a port it found taken is.
fn holders_of(port: u16) -> String {
    let owned = lsof(&["-nP", &format!("-iTCP:{port}"), "-Fpcn"]).map_or_else(
        || "(no lsof)".into(),
        |o| String::from_utf8_lossy(&o.stdout).replace('\n', " "),
    );
    // And what no process owns, which lsof cannot see: connections in TIME_WAIT, say.
    let kernel: String = std::process::Command::new("netstat")
        .args(["-an", "-p", "tcp"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.contains(&format!(".{port} ")) || l.contains(&format!(":{port} ")))
                .map(|l| format!("{}; ", l.split_whitespace().collect::<Vec<_>>().join(" ")))
                .collect()
        })
        .unwrap_or_default();
    format!("[{owned}] netstat: [{kernel}]")
}

/// Who holds `port` while it is held: the system's sockets on it, asked at once and again
/// until a bind of it succeeds (up to 2 s), with how long that took. A holder that lives
/// a few milliseconds is gone before lsof, which reads every process, has answered
/// (project-port-free-flake), so netstat (with each socket's process, `-v`) or ss go
/// first.
fn catch_holder(port: u16) -> String {
    let began = Instant::now();
    let mut seen: Vec<String> = Vec::new();
    let sockets = || -> String {
        let out = if cfg!(target_os = "macos") {
            std::process::Command::new("netstat")
                .args(["-anv", "-p", "tcp"])
                .output()
        } else {
            std::process::Command::new("ss").args(["-tanpe"]).output()
        };
        out.map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.contains(&format!(".{port} ")) || l.contains(&format!(":{port} ")))
                .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_else(|e| format!("({e})"))
    };
    while began.elapsed() < Duration::from_secs(2) {
        let now = sockets();
        if seen.len() < 5 && !now.is_empty() && seen.last() != Some(&now) {
            seen.push(format!("{:?}: {now}", began.elapsed()));
        }
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return format!(
                "free after {:?}; sockets seen: [{}]",
                began.elapsed(),
                seen.join(" | ")
            );
        }
    }
    format!(
        "held 2 s and more; sockets seen: [{}]; {}",
        seen.join(" | "),
        holders_of(port)
    )
}

/// lsof's answer to `args`, where the host's lsof is lsof's own, which `-v` names: none
/// where it has none, or busybox's (Alpine's), which ignores what it is asked and lists
/// every open file.
fn lsof(args: &[&str]) -> Option<std::process::Output> {
    let v = std::process::Command::new("lsof").arg("-v").output().ok()?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&v.stdout),
        String::from_utf8_lossy(&v.stderr)
    );
    if !said.contains("lsof version") {
        return None;
    }
    std::process::Command::new("lsof").args(args).output().ok()
}

/// The processes listening on TCP port `port`, as lsof lists them: empty where none does
/// (or where the host has no lsof of its own).
fn listeners_of(port: u16) -> String {
    lsof(&["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpcn"])
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .replace('\n', " ")
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

/// `-p`: a microVM's ports published on the host as dockerd publishes them. Each
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
    // The listening sockets are the run's network process's alone: the daemon lets go of
    // its copies once the VM has the run (M24), and a copy kept would hold the port after
    // the run's end is told, until the daemon had reaped both of its processes.
    // lsof's own (`-F` fields, exit 1 when nothing matches), where the host has it.
    let lsof = || lsof(&["-nP", &format!("-iTCP:{n}"), "-sTCP:LISTEN", "-Fp"]);
    let holders = |o: &std::process::Output| -> Vec<String> {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix('p').map(String::from))
            .collect()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut seen = lsof();
    while let Some(o) = &seen
        && holders(o).is_empty()
        && !o.status.success()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
        seen = lsof();
    }
    if let Some(lsof) = seen {
        let log = std::fs::read_to_string(home.join("daemon.log")).unwrap();
        let daemon = log
            .lines()
            .find_map(|l| l.strip_prefix("shards daemon ")?.split(':').next())
            .unwrap()
            .to_string();
        let holders = holders(&lsof);
        assert!(
            !holders.is_empty(),
            "nothing listens on {n}: lsof {}\n{}{}",
            lsof.status,
            String::from_utf8_lossy(&lsof.stdout),
            String::from_utf8_lossy(&lsof.stderr)
        );
        assert!(
            !holders.contains(&daemon),
            "the daemon {daemon} holds {n}: {holders:?}"
        );
    }
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
        assert_eq!(from, format!("from {}\n", bridge().gateway()));
        if echoed != big {
            // Where the echo first differs, and what the guest's server said: an end of its
            // own (`serve error`), or none, which leaves the bytes to the network process.
            let differs = echoed.iter().zip(&big).position(|(a, b)| a != b);
            let logs = shards(&["logs", "web"]);
            let net: String = std::fs::read_to_string(home.join("daemon.log"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("shards-net"))
                .map(|l| format!("{l}\n"))
                .collect();
            panic!(
                "{} bytes came back of {}, first differing at {differs:?}; the guest said:\n{}{}\nthe network process said:\n{net}",
                echoed.len(),
                big.len(),
                logs.stdout,
                logs.stderr
            );
        }
    }
    let (from, echoed) = exchange(format!("[::1]:{n}").parse().unwrap(), b"over IPv6".to_vec());
    assert_eq!(
        (from.as_str(), echoed.as_slice()),
        (
            format!("from {}\n", bridge().gateway()).as_str(),
            &b"over IPv6"[..]
        )
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
    // And at one address of the family a running container holds the port at every
    // address of: refused as dockerd's allocator refuses it, where BSD's SO_REUSEADDR
    // would have let the bind in (review 2.7).
    let narrow = run_in(
        &home,
        &image,
        &["--name", "narrow", "-p", &format!("127.0.0.1:{n}:7000")],
        &["exit", "0"],
    );
    assert_eq!(narrow.status, Some(125), "{narrow}");
    assert!(
        narrow.stderr.contains(&format!(
            "Bind for 127.0.0.1:{n} failed: port is already allocated"
        )),
        "{narrow}"
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
        (format!("from {}\n", bridge().gateway()).as_str(), &b"last"[..])
    );
    assert_eq!(exit(&mut run), Some(0));
    // Ended, it lists none, and its port is free at once.
    let gone = shards(&["port", "web"]);
    assert_eq!((gone.status, gone.stdout.as_str()), (Some(0), ""), "{gone}");
    // Each run's port is free as soon as `run` returns, as `docker run`'s is: the next
    // program to bind it has it, every time. A port below the ephemeral ranges, which no
    // other test's connection can be given between the runs.
    let p = fixed_port();
    // When this test last had p itself, bound and let go: a run refused after it says how
    // long another program had to take it (project-port-free-flake).
    let mut freed = Instant::now();
    for i in 0..20 {
        let again = run_in(
            &home,
            &image,
            &["--rm", "-p", &format!("{p}:7000")],
            &["exit", "0"],
        );
        assert_eq!(
            again.status,
            Some(0),
            "{again}; run {i} of 20, {:?} after this test had {p} free; {}",
            freed.elapsed(),
            catch_holder(p)
        );
        // Nothing listens on it once `run` has returned: asked of the system, not by
        // binding, which SO_REUSEADDR lets succeed beside a socket a later bind meets.
        let listening = listeners_of(p);
        assert!(
            listening.is_empty(),
            "{p} after a run ended: still listened on by {listening}"
        );
        if let Err(e) = std::net::TcpListener::bind(("0.0.0.0", p)) {
            panic!("{p} after a run ended: {e}; {}", catch_holder(p));
        }
        freed = Instant::now();
    }
    // And the daemon holds it no longer: taken by another program, it is refused at once,
    // not after the wait for a run's ports to come free.
    let held = std::net::TcpListener::bind(("0.0.0.0", p))
        .unwrap_or_else(|e| panic!("{p}: {e}; {}", catch_holder(p)));
    let began = Instant::now();
    let refused = run_in(
        &home,
        &image,
        &["--rm", "-p", &format!("{p}:7000")],
        &["exit", "0"],
    );
    assert!(
        refused.stderr.contains(&format!(
            "): failed to bind host port 0.0.0.0:{p}/tcp: address already in use"
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
    // An answer that never comes says what each side saw: the daemon's log, the run's
    // own output, and the host's UDP sockets on the port (a lost datagram, 2026-10-06,
    // under the full suite's load).
    let ask = |client: &UdpSocket, to: std::net::SocketAddr, payload: &[u8]| {
        client.set_read_timeout(Some(TIMEOUT)).unwrap();
        client.send_to(payload, to).unwrap();
        let mut buf = [0u8; 2048];
        let (len, from) = common::recv_from(client, &mut buf).unwrap_or_else(|e| {
            let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(60).collect();
            let logs = shards_in(&home, &["logs", "dns"]);
            let sockets = std::process::Command::new("netstat")
                .args(["-an", "-p", "udp"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let port = to.port().to_string();
            let ours: Vec<&str> = sockets.lines().filter(|l| l.contains(&port)).collect();
            panic!(
                "no answer to {payload:?} from {to}: {e}\n--- daemon.log (last lines first)\n{}\n--- logs\n{logs}\n--- netstat {port}\n{}",
                tail.join("\n"),
                ours.join("\n")
            )
        });
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
            .strip_prefix(&format!("from {}:", bridge().gateway()))
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
    // As `docker pull` says it, measured on Docker 29.3.1: the pull named once the tag
    // resolved; a tag the registry lacks refused in dockerd's words, with nothing said
    // to be pulling.
    assert!(
        pulled.stdout.starts_with("v1: Pulling from test/image\n"),
        "{pulled}"
    );
    let missing = format!("127.0.0.1:{port}/test/image:nosuch");
    let refused = shards(&["pull", &missing]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert_eq!(refused.stdout, "", "{refused}");
    assert_eq!(
        refused.stderr,
        format!(
            "Error response from daemon: failed to resolve reference \"{missing}\": {missing}: not found\n"
        ),
        "{refused}"
    );
    let short = id.strip_prefix("sha256:").and_then(|hex| hex.get(..12)).unwrap();
    // What is here of it, as dockerd counts it: its manifests' blobs, not the index's; and
    // its root filesystem.
    let dir_bytes = |dir: &Path| -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum()
    };
    let content = dir_bytes(&home.join("images/blobs/sha256")) - index.len() as u64;
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
    // A pattern, matched as Go's path.Match matches the name familiar or whole, with its
    // tag or without (docker-v29.8.1 setupFilters; 29.3.1 matched the familiar alone).
    let none = shards(&["images", "nothing"]);
    assert_eq!(none.stdout.lines().count(), 1, "{none}");
    let some = shards(&["image", "ls", "127.0.0.1:*/test/*"]);
    assert_eq!(some.stdout, listed.stdout, "{some}");
    // `*` stops at `/`: no name of it is one segment.
    assert_eq!(shards(&["image", "list", "*:v1"]).stdout.lines().count(), 1);
    // On Docker Hub the familiar name is the short one, the whole one the long; a middle
    // one is neither.
    assert_eq!(shards(&["tag", &image, "hubbish:1"]).status, Some(0));
    for (pattern, rows) in [
        ("hubbish", 2),
        ("hubbish:1", 2),
        ("hub*", 2),
        ("docker.io/library/hubbish", 2),
        ("docker.io/library/hubbish:1", 2),
        ("library/hubbish", 1),
    ] {
        let listed = shards(&["images", pattern]);
        assert_eq!(listed.stdout.lines().count(), rows, "{pattern}: {listed}");
    }
    assert_eq!(shards(&["rmi", "hubbish:1"]).status, Some(0));
    // Pulled by digest too: that name is one of its names, listed on a row of its own as
    // dockerd lists it (tagsByDigest); the table shows tags, so no row for it there.
    let by_digest = format!("{repo}@{id}");
    let pulled = shards(&["pull", "-q", &by_digest]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let names: Vec<String> = shards(&["images"])
        .stdout
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect();
    assert_eq!(names, [image.clone(), by_digest.clone()]);
    let table = shards(&["images", "--digests"]);
    assert_eq!(table.stdout.lines().count(), 2, "{table}");
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
    // Removing a name by digest removes every name of its repository with it, as dockerd's
    // getSameReferences does: here all of the image's, so the image goes too (measured,
    // Docker 29.3.1).
    let removed = shards(&["rmi", &by_digest]);
    assert_eq!(
        removed.stdout,
        format!("Untagged: {image}\nUntagged: {by_digest}\nDeleted: {id}\n"),
        "{removed}"
    );
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

/// An interrupted `shards save -o` leaves nothing behind: its temporary file goes and its
/// destination is never made, where Docker 29.3.1 renames what an interrupted save wrote
/// into place (measured). Its daemon here takes the request and never answers, so the
/// signal comes while the save is under way. The signals' default actions are restored in
/// the client, which a test run under `cmd &` would otherwise start ignoring SIGINT.
#[test]
fn an_interrupted_save_leaves_nothing_behind() {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    let home = TempDir::new("containers-save-interrupted");
    let _daemon = std::os::unix::net::UnixListener::bind(home.join("daemon.sock")).unwrap();
    let out = TempDir::new("containers-save-interrupted-out");
    let dest = out.join("image.tar");
    let left = || -> Vec<std::ffi::OsString> {
        std::fs::read_dir(&*out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    };
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let mut save = common::command();
        save.args(["save", "-o", dest.to_str().unwrap(), "any:1"])
            .env("SHARDS_HOME", &*home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: signal(2), async-signal-safe, between fork and exec.
        unsafe {
            save.pre_exec(|| {
                for s in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
                    libc::signal(s, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let mut save = save.spawn().unwrap();
        eventually("the save's temporary file", || !left().is_empty());
        // SAFETY: kill(2) of the client this test started.
        assert_eq!(unsafe { libc::kill(save.id() as libc::pid_t, sig) }, 0);
        let status = save.wait().unwrap();
        assert_eq!(status.signal(), Some(sig), "{status}");
        assert_eq!(left(), Vec::<std::ffi::OsString>::new(), "after signal {sig}");
    }
    // A client that can reach no daemon fails, and leaves nothing either: its home is a
    // file, where no daemon can be.
    let nowhere = out.join("not-a-home");
    std::fs::write(&nowhere, b"").unwrap();
    let failed = common::command()
        .args(["save", "-o", dest.to_str().unwrap(), "any:1"])
        .env("SHARDS_HOME", &nowhere)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    std::fs::remove_file(&nowhere).unwrap();
    assert_ne!(failed.status.code(), Some(0));
    assert_eq!(left(), Vec::<std::ffi::OsString>::new());
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
    // 0600 whatever the umask, as atomicwriter's Close chmods it: made under umask 0377,
    // the file would be 0400.
    let strict = out.join("strict.tar");
    let mut save = common::command();
    save.args(["save", "-o", strict.to_str().unwrap(), &image])
        .env("SHARDS_HOME", &*home)
        .stdout(Stdio::null());
    // SAFETY: umask(2), async-signal-safe, between fork and exec.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut save, || {
            libc::umask(0o377);
            Ok(())
        });
    }
    assert!(save.status().unwrap().success());
    assert_eq!(
        std::fs::metadata(&strict).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&strict).unwrap();
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
    let piped = common::command()
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
/// stdin, and compressed with bzip2, xz and zstd; Docker's older layout, manifest.json and
/// `<id>/layer.tar`; an image with no name by its ID; and a broken archive refused in Go's
/// words.
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
    let mut load = common::command()
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
    // Every compression go-archive's DecompressStream reads, as dockerd loads them
    // (measured, Docker 29.3.1): bzip2, xz, and zstd begun by a skippable frame.
    let archive = std::fs::read(&saved_tar).unwrap();
    let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
    bz.write_all(&archive).unwrap();
    let mut xz = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
    xz.write_all(&archive).unwrap();
    let mut zst = vec![0x5f, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3];
    zst.extend(ruzstd::encoding::compress_to_vec(
        &archive[..],
        ruzstd::encoding::CompressionLevel::Fastest,
    ));
    for (name, compressed) in [
        ("bzip2", bz.finish().unwrap()),
        ("xz", xz.finish().unwrap()),
        ("zstd", zst),
    ] {
        let path = first.join(format!("image.tar.{name}"));
        std::fs::write(&path, compressed).unwrap();
        let home = TempDir::new(&format!("containers-load-{name}"));
        let loaded = shards_in(&home, &["load", "-i", path.to_str().unwrap()]);
        assert_eq!(
            (loaded.status, loaded.stdout.as_str()),
            (Some(0), format!("Loaded image: {image}\n").as_str()),
            "{name}: {loaded}"
        );
    }
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
/// A registry that has no blob and never answers an upload: what a push waits on. Counts
/// the uploads begun, and those whose connection ended.
fn stalling_registry() -> (
    u16,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (begun, ended) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (b, e) = (begun.clone(), ended.clone());
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(mut tcp) = tcp else { return };
            let (b, e) = (b.clone(), e.clone());
            std::thread::spawn(move || {
                let mut first = [0u8; 1];
                if tcp.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                    let _ = tcp.write_all(common::GO_BAD_REQUEST);
                    return;
                }
                loop {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match tcp.read(&mut byte) {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    if head.starts_with(b"POST ") {
                        b.fetch_add(1, Ordering::SeqCst);
                        // Never answered: held until the client lets it go.
                        while matches!(tcp.read(&mut byte), Ok(1)) {}
                        e.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    let _ = tcp.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                }
            });
        }
    });
    (port, begun, ended)
}

/// A push in flight ends with the daemon, at once, and with its client: its uploads stop
/// as its connection to the registry closes, where they waited on the registry before.
#[test]
fn a_push_ends_with_its_client_and_the_daemon() {
    use std::sync::atomic::Ordering;
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    let (from, _) = registry(index, blobs);
    let source = format!("127.0.0.1:{from}/test/image:v1");
    let (port, begun, ended) = stalling_registry();
    let home = TempDir::new("containers-push-ends");
    let shards = |args: &[&str]| shards_in(&home, args);
    assert_eq!(shards(&["pull", "-q", &source]).status, Some(0));
    let target = format!("127.0.0.1:{port}/team/app:1");
    assert_eq!(shards(&["tag", &source, &target]).status, Some(0));
    let push = || {
        common::command()
            .args(["push", &target])
            .env("SHARDS_HOME", &*home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    // Its uploads, the config's and the layer's, let go within 5 s: at once, not when an
    // upload's 30 s wait for an answer runs out.
    let let_go = |what: &str| {
        let t0 = Instant::now();
        while ended.load(Ordering::SeqCst) < begun.load(Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(5), "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    // Its client killed: its uploads' connections end.
    let mut client = push();
    eventually("the uploads begun", || begun.load(Ordering::SeqCst) >= 1);
    client.kill().unwrap();
    let _ = client.wait();
    let_go("the uploads went on without their client");
    // The daemon stopped: it stops at once, and so does the push.
    let first = begun.load(Ordering::SeqCst);
    let mut client = push();
    eventually("the second push's uploads begun", || {
        begun.load(Ordering::SeqCst) > first
    });
    let t0 = Instant::now();
    let stopped = shards(&["daemon", "stop"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let status = client.wait().unwrap();
    assert!(!status.success(), "{status}");
    let_go("the uploads went on past the daemon");
}

/// A run interrupted as it pulls ends as `docker run` does, measured on Docker 29.3.1:
/// at once, with 130 and nothing more said; its pull let go, and neither a container nor
/// the image left. The client starts with SIGINT's default action, as a terminal's
/// session gives it: a test run under `cmd &` would otherwise start it ignoring SIGINT
/// (POSIX.1-2024, XCU 2.9.3.1), and an ignored SIGINT ends no client, as it ends no
/// Docker CLI.
#[test]
fn a_run_interrupted_as_it_pulls_ends_as_docker_runs_do() {
    use std::os::unix::process::CommandExt as _;
    use std::sync::atomic::Ordering;
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    let (port, begun, ended) = common::registry_stalling_blobs(index, blobs);
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let home = TempDir::new("containers-run-interrupted");
    let mut client = common::command();
    // SAFETY: signal(2), async-signal-safe, between fork and exec.
    unsafe {
        client.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    let client = client
        .args(["run", &image, "true"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    eventually("the pull's blobs asked for", || begun.load(Ordering::SeqCst) >= 1);
    let pid = libc::pid_t::try_from(client.id()).unwrap();
    // SAFETY: kill(2) of the client this test started, not yet waited for.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let t0 = Instant::now();
    let out = client.wait_with_output().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{stderr}");
    assert_eq!(
        stderr,
        format!("Unable to find image '{image}' locally\nv1: Pulling from test/image\n"),
        "{}",
        std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
    );
    assert!(out.stdout.is_empty());
    // Its pull let go: the registry's connections end.
    let t0 = Instant::now();
    while ended.load(Ordering::SeqCst) < begun.load(Ordering::SeqCst) {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the pull went on without its client"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(shards_in(&home, &["ps", "-aq"]).stdout, "");
    assert_eq!(shards_in(&home, &["images", "-q"]).stdout, "");
}

/// A push goes with the credentials of the client that asks for it, as the Docker CLI
/// sends its own with each request: not those of the client that started the daemon.
#[test]
fn a_push_goes_with_its_clients_credentials() {
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    let (from, _) = registry(index, blobs);
    let source = format!("127.0.0.1:{from}/test/image:v1");
    // base64("shards:secret")
    let (port, repos) = common::writable_registry_requiring(Some("Basic c2hhcmRzOnNlY3JldA==".into()));
    let home = TempDir::new("containers-push-credentials");
    let (none, some) = (TempDir::new("push-config-none"), TempDir::new("push-config-some"));
    std::fs::write(
        some.join("config.json"),
        format!(r#"{{"auths":{{"127.0.0.1:{port}":{{"auth":"c2hhcmRzOnNlY3JldA=="}}}}}}"#),
    )
    .unwrap();
    let (kernel, init) = (kernel(), guest_init());
    let shards = |config: &Path, args: &[&str]| {
        let env: [(&str, &OsStr); 4] = [
            ("SHARDS_HOME", home.as_os_str()),
            ("DOCKER_CONFIG", config.as_os_str()),
            ("SHARDS_KERNEL", kernel.as_os_str()),
            ("SHARDS_INIT", init.as_os_str()),
        ];
        run_shards_env(&[], args, &env, TIMEOUT)
    };
    // The daemon starts from a client with no credentials.
    assert_eq!(shards(&none, &["pull", "-q", &source]).status, Some(0));
    let target = format!("127.0.0.1:{port}/team/app:1");
    assert_eq!(shards(&none, &["tag", &source, &target]).status, Some(0));
    // Refused as Docker 29.3.1 reports it, measured against registry:2 with basic auth.
    let refused = shards(&none, &["push", &target]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused.stderr.ends_with(
            "push access denied, repository does not exist or may require authorization: \
             authorization failed: no basic auth credentials\n"
        ),
        "{refused}"
    );
    let pushed = shards(&some, &["push", &target]);
    assert_eq!(pushed.status, Some(0), "{pushed}");
    // Every tag of the repository pushed by one client: challenged only as its first
    // requests, its two blobs' checks at once, meet the registry; not again per tag.
    for tag in ["2", "3"] {
        let more = format!("127.0.0.1:{port}/team/app:{tag}");
        assert_eq!(shards(&none, &["tag", &source, &more]).status, Some(0));
    }
    let challenged = || {
        repos
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.starts_with("401 "))
            .count()
    };
    let before = challenged();
    let all = shards(&some, &["push", "-a", &format!("127.0.0.1:{port}/team/app")]);
    assert_eq!(all.status, Some(0), "{all}");
    assert!(challenged() - before <= 2, "{:?}", repos.lock().unwrap().log);
    // Tag 1 named the same manifest already: asked for with a HEAD, and not put again,
    // as containerd's pusher leaves it.
    let puts = repos
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|l| *l == "PUT /v2/team/app/manifests/1")
        .count();
    assert_eq!(puts, 1, "{:?}", repos.lock().unwrap().log);
    // A run's pull too.
    assert_eq!(shards(&none, &["rmi", &target]).status, Some(0));
    let refused = shards(&none, &["run", "--rm", &target, "exit", "0"]);
    assert_ne!(refused.status, Some(0), "{refused}");
    let ran = shards(&some, &["run", "--rm", &target, "exit", "0"]);
    assert_eq!(ran.status, Some(0), "{ran}");
}

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

/// `-v` and `--mount`, as `docker run` takes them (D38): a host directory shared both ways
/// and owned as Docker Desktop records it, read-only where asked, a file bound alone; a
/// named volume filled from the image where it is empty, and kept; tmpfs; inspect's
/// mounts; anonymous volumes removed with `--rm` and `rm -v`, and kept by `rm`.
#[cfg(unix)]
#[test]
fn run_mounts_binds_volumes_and_tmpfs_as_docker_run_does() {
    let Some((home, image)) = home("containers-volumes") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let shared = home.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("a"), "host").unwrap();
    let file = home.join("one");
    std::fs::write(&file, "alone").unwrap();
    let bind = format!("{}:/data", shared.display());
    let ro = format!("{}:/ro:ro", shared.display());
    let single = format!("{}:/etc/one:ro", file.display());
    let ran = run_in(
        &home,
        &image,
        &[
            "--rm",
            "-v",
            &bind,
            "-v",
            &ro,
            "-v",
            &single,
            "-v",
            "tools:/bin",
            "--mount",
            "type=tmpfs,dst=/t,tmpfs-size=1m",
        ],
        &["stat", "/data/a", "/etc/one", "/proc/self/mounts"],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    assert!(ran.stdout.contains("/data/a file 644 0:0 4\n= host\n"), "{ran}");
    assert!(ran.stdout.contains("/etc/one file 644 0:0 5\n= alone\n"), "{ran}");
    for mount in [
        "shards0 /data virtiofs rw,",
        "shards1 /ro virtiofs ro,",
        "shards2 /etc/one virtiofs ro,",
        "shards3 /bin virtiofs rw,",
        "tmpfs /t tmpfs rw,nosuid,nodev,noexec,relatime,size=1024k",
    ] {
        assert!(ran.stdout.contains(mount), "{mount}: {ran}");
    }
    // Written in the guest, there on the host, owned as the guest made it; read-only
    // where asked; the file alone, nothing else of its directory.
    let wrote = run_in(
        &home,
        &image,
        &["--rm", "-v", &bind, "-v", &ro, "-u", "1000:1000"],
        &["fs", "mkdir:/data/d", "write:/data/d/b=guest"],
    );
    assert_eq!(
        wrote.status,
        Some(1),
        "a directory root owns refuses user 1000: {wrote}"
    );
    // The image's user is `app`; root makes the directory.
    let wrote = run_in(
        &home,
        &image,
        &["--rm", "-u", "0", "-v", &bind],
        &[
            "fs",
            "mkdir:/data/d",
            "write:/data/d/b=guest",
            "chmod:1777:/data/d",
        ],
    );
    assert_eq!(wrote.status, Some(0), "{wrote}");
    assert_eq!(std::fs::read_to_string(shared.join("d/b")).unwrap(), "guest");
    let user = run_in(
        &home,
        &image,
        &["--rm", "-v", &bind, "-u", "1000:1000"],
        &["fs", "write:/data/d/c=user"],
    );
    assert_eq!(user.status, Some(0), "{user}");
    let owned = run_in(
        &home,
        &image,
        &["--rm", "-v", &bind],
        &["stat", "/data/d/c", "/data/d"],
    );
    assert!(
        owned.stdout.contains("/data/d/c file 644 1000:1000 4\n"),
        "{owned}"
    );
    assert!(owned.stdout.contains("/data/d dir 1777 0:0"), "{owned}");
    let refused = run_in(&home, &image, &["--rm", "-v", &ro], &["fs", "write:/ro/x=1"]);
    assert!(
        refused.stderr.contains("write:/ro/x=1: Read-only file system"),
        "{refused}"
    );
    let alone = run_in(
        &home,
        &image,
        &["--rm", "-v", &single],
        &["stat", "/etc/one/../a"],
    );
    assert_eq!(alone.status, Some(1), "{alone}");
    // The named volume keeps what the image had at /bin, copied as it was first mounted.
    let kept = home.join("volumes/tools/_data/testguest");
    assert!(kept.exists(), "the image's /bin in the volume");
    let again = run_in(
        &home,
        &image,
        &["--rm", "-v", "tools:/opt"],
        &["stat", "/opt/testguest"],
    );
    assert_eq!(again.status, Some(0), "{again}");
    // Inspect: dockerd's fields.
    let made = shards(&[
        "create",
        "--name",
        "held",
        "-v",
        &bind,
        "-v",
        "/anon",
        "--mount",
        "type=volume,src=named,dst=/v",
        &image,
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let shown = shards(&[
        "inspect",
        "-f",
        "{{range .Mounts}}{{.Type}} {{.Destination}} {{.RW}} {{.Mode}} {{.Propagation}};{{end}} {{json .HostConfig.Binds}} {{json .Config.Volumes}}",
        "held",
    ]);
    assert_eq!(
        shown.stdout,
        format!(
            "volume /anon true  ;bind /data true  rprivate;volume /v true z ; [\"{}\"] {{\"/anon\":{{}}}}\n",
            bind
        ),
        "{shown}"
    );
    // Anonymous volumes: `rm` keeps them, `rm -v` and `--rm` do not; named ones stay.
    let volumes = || {
        std::fs::read_dir(home.join("volumes"))
            .unwrap()
            .filter(|e| !e.as_ref().unwrap().file_name().to_string_lossy().starts_with('.'))
            .count()
    };
    let before = volumes();
    assert_eq!(shards(&["rm", "-v", "held"]).status, Some(0));
    assert_eq!(volumes(), before - 1, "rm -v removes the anonymous volume alone");
    let anon = run_in(&home, &image, &["--rm", "-v", "/anon"], &["stat", "/anon"]);
    assert_eq!(anon.status, Some(0), "{anon}");
    // Gone by the next command that reads volumes, as by the time `docker run --rm`
    // returns.
    let listed = shards(&["volume", "ls", "-q"]);
    assert_eq!(listed.stdout.lines().count(), before - 1, "{listed}");
    assert_eq!(volumes(), before - 1, "--rm removes its anonymous volume");
    let made = shards(&["create", "--name", "kept", "-v", "/anon", &image]);
    assert_eq!(made.status, Some(0), "{made}");
    assert_eq!(shards(&["rm", "kept"]).status, Some(0));
    assert_eq!(volumes(), before, "rm keeps it");
}

/// `volume create`, `ls`, `inspect`, `rm` and `prune`, as `docker volume` says them: a
/// volume made, mounted, refused removal while a microVM mounts it, listed, pruned; a
/// local volume with `o=bind` mounts its host directory; `system df` counts them.
#[cfg(unix)]
#[test]
fn volume_commands_keep_volumes_as_docker_volume_does() {
    let Some((home, image)) = home("containers-volume-commands") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = shards(&["volume", "create", "--label", "team=a", "data"]);
    assert_eq!((made.status, made.stdout.as_str()), (Some(0), "data\n"), "{made}");
    // Made again, it is the same volume.
    assert_eq!(shards(&["volume", "create", "data"]).stdout, "data\n");
    let refused = shards(&["volume", "create", "-o", "bad=1", "other"]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: create other: invalid option: \"bad\"\n"
        )
    );
    // Written by one microVM, read by the next.
    let wrote = run_in(
        &home,
        &image,
        &["--rm", "-u", "0", "-v", "data:/d"],
        &["fs", "write:/d/f=kept"],
    );
    assert_eq!(wrote.status, Some(0), "{wrote}");
    let read = run_in(&home, &image, &["--rm", "-v", "data:/d"], &["stat", "/d/f"]);
    assert!(read.stdout.contains("= kept\n"), "{read}");
    // In use while a microVM mounts it, stopped or not.
    assert_eq!(
        shards(&["create", "--name", "holder", "-v", "data:/d", &image]).status,
        Some(0)
    );
    let id = shards(&["inspect", "-f", "{{.Id}}", "holder"])
        .stdout
        .trim()
        .to_string();
    let in_use = shards(&["volume", "rm", "data"]);
    assert_eq!(
        (in_use.status, in_use.stderr),
        (
            Some(1),
            format!("Error response from daemon: remove data: volume is in use - [{id}]\n")
        )
    );
    let listed = shards(&[
        "volume",
        "ls",
        "-f",
        "dangling=false",
        "--format",
        "{{.Driver}} {{.Name}} {{.Labels}}",
    ]);
    assert_eq!(listed.stdout, "local data team=a\n", "{listed}");
    let shown = shards(&[
        "volume",
        "inspect",
        "-f",
        "{{.Name}} {{.Driver}} {{.Scope}} {{json .Labels}} {{json .Options}}",
        "data",
    ]);
    assert_eq!(
        shown.stdout, "data local local {\"team\":\"a\"} null\n",
        "{shown}"
    );
    let missing = shards(&["volume", "inspect", "nope"]);
    assert_eq!(
        (missing.status, missing.stdout.as_str(), missing.stderr.as_str()),
        (
            Some(1),
            "[]\n",
            "Error response from daemon: get nope: no such volume\n"
        )
    );
    // system df counts it, in use.
    let df = shards(&[
        "system",
        "df",
        "--format",
        "{{.Type}} {{.TotalCount}} {{.Active}}",
    ]);
    assert!(df.stdout.contains("Local Volumes 1 1\n"), "{df}");
    assert_eq!(shards(&["rm", "holder"]).status, Some(0));
    // Pruned: anonymous volumes alone, then all with -a.
    assert_eq!(shards(&["volume", "create"]).status, Some(0));
    let pruned = shards(&["volume", "prune", "-f"]);
    assert!(
        pruned.stdout.starts_with("Deleted Volumes:\n") && !pruned.stdout.contains("\ndata\n"),
        "{pruned}"
    );
    let all = shards(&["volume", "prune", "-af"]);
    assert!(
        all.stdout
            .starts_with("Deleted Volumes:\ndata\n\nTotal reclaimed space: "),
        "{all}"
    );
    assert_eq!(shards(&["volume", "ls", "-q"]).stdout, "");
    // A local volume of a host directory, bound.
    let host = home.join("bound");
    std::fs::create_dir(&host).unwrap();
    std::fs::write(host.join("h"), "host").unwrap();
    let device = format!("device={}", host.display());
    let bound = shards(&[
        "volume",
        "create",
        "-o",
        "type=none",
        "-o",
        "o=bind",
        "-o",
        &device,
        "hostdir",
    ]);
    assert_eq!(bound.status, Some(0), "{bound}");
    let through = run_in(&home, &image, &["--rm", "-v", "hostdir:/h"], &["stat", "/h/h"]);
    assert!(through.stdout.contains("= host\n"), "{through}");
    // Shards' grammar says the same.
    assert_eq!(shards(&["list", "volumes", "-q"]).stdout, "hostdir\n");
    assert_eq!(shards(&["remove", "volume", "hostdir"]).stdout, "hostdir\n");
    assert!(host.join("h").exists(), "a bound directory outlives its volume");
}

/// `--restart`, as dockerd's restart manager keeps it: `on-failure:N` restarts a failing
/// command N times; `always` until stopped, a stop ending it for good; as the daemon
/// starts again, `unless-stopped` and `always` start again what its stop ended, and not
/// what was stopped by hand.
#[cfg(unix)]
#[test]
fn restart_policies_start_containers_again_as_dockerd_does() {
    let Some((home, image)) = home("containers-restart") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let inspect =
        |name: &str, format: &str| shards(&["inspect", "-f", format, name]).stdout.trim().to_string();
    let until = |what: &str, check: &dyn Fn() -> bool| {
        let deadline = Instant::now() + TIMEOUT;
        while !check() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let failing = run_in(
        &home,
        &image,
        &["-d", "--name", "failing", "--restart", "on-failure:2"],
        &["exit", "3"],
    );
    assert_eq!(failing.status, Some(0), "{failing}");
    until("on-failure:2 restarts twice, then stays exited", &|| {
        inspect("failing", "{{.RestartCount}} {{.State.Status}}") == "2 exited"
    });
    assert_eq!(
        inspect(
            "failing",
            "{{.State.ExitCode}} {{json .HostConfig.RestartPolicy}}"
        ),
        "3 {\"Name\":\"on-failure\",\"MaximumRetryCount\":2}"
    );
    // always: restarting between its runs, as ps and inspect say, until stopped.
    let always = run_in(
        &home,
        &image,
        &["-d", "--name", "always", "--restart", "always"],
        &["exit", "1"],
    );
    assert_eq!(always.status, Some(0), "{always}");
    until("always restarts, and waits longer each time", &|| {
        inspect("always", "{{.RestartCount}}").parse::<u64>().unwrap_or(0) >= 3
    });
    until("it is seen waiting to restart", &|| {
        inspect(
            "always",
            "{{.State.Status}} {{.State.Running}} {{.State.Restarting}}",
        ) == "restarting true true"
    });
    let listed = shards(&["ps", "--filter", "name=always", "--format", "{{.Status}}"]);
    assert!(listed.stdout.starts_with("Restarting (1) "), "{listed}");
    assert_eq!(shards(&["stop", "always"]).stdout, "always\n");
    let count = inspect("always", "{{.RestartCount}}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(inspect("always", "{{.State.Status}}"), "exited");
    assert_eq!(
        inspect("always", "{{.RestartCount}}"),
        count,
        "stopped, it restarts no more"
    );
    // The daemon's stop ends both; as it starts again, the one stopped by it starts, and
    // the one stopped by hand does not.
    let mut kept = start(
        &home,
        &image,
        &["--name", "kept", "--restart", "unless-stopped"],
        &["sleep"],
    );
    let mut held = start(
        &home,
        &image,
        &["--name", "held", "--restart", "unless-stopped"],
        &["sleep"],
    );
    assert_eq!(shards(&["stop", "-t", "0", "held"]).status, Some(0));
    // SIGTERM, then SIGKILL at once: whichever ends it.
    assert!(matches!(exit(&mut held), Some(137 | 143)));
    assert_eq!(shards(&["stop", "daemon"]).status, Some(0));
    let _ = exit(&mut kept);
    until("the daemon starts kept again", &|| {
        inspect("kept", "{{.State.Status}}") == "running"
    });
    assert_eq!(inspect("held", "{{.State.Status}}"), "exited");
    assert_eq!(
        shards(&["rm", "-f", "kept", "held", "always", "failing"]).status,
        Some(0)
    );
}

/// `update`, as `docker update` takes it: a running microVM's limits written to its
/// workload's cgroup as it runs, and kept; dockerd's refusals; the restart policy.
#[cfg(unix)]
#[test]
fn update_changes_limits_of_running_microvms_as_docker_update_does() {
    let Some((home, image)) = home("containers-update") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let mut held = start(&home, &image, &["--name", "held"], &["sleep"]);
    let id = shards(&["inspect", "-f", "{{.Id}}", "held"])
        .stdout
        .trim()
        .to_string();
    let refused = shards(&["update", "-m", "64m", "held"]);
    assert_eq!(
        (refused.status, refused.stderr),
        (
            Some(1),
            format!(
                "Error response from daemon: Cannot update container {id}: Memory limit should be smaller than already set memoryswap limit, update the memoryswap at the same time\n"
            )
        )
    );
    let updated = shards(&[
        "update",
        "-m",
        "64m",
        "--memory-swap",
        "128m",
        "--cpus",
        "1.5",
        "--pids-limit",
        "50",
        "--blkio-weight",
        "300",
        "--restart",
        "always",
        "held",
    ]);
    assert_eq!(
        (updated.status, updated.stdout.as_str()),
        (Some(0), "held\n"),
        "{updated}"
    );
    let read = shards(&[
        "exec",
        "held",
        "/bin/testguest",
        "stat",
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory.swap.max",
        "/sys/fs/cgroup/cpu.max",
        "/sys/fs/cgroup/pids.max",
    ]);
    let values: Vec<&str> = read.stdout.lines().filter_map(|l| l.strip_prefix("= ")).collect();
    assert_eq!(
        values,
        ["67108864\\n", "67108864\\n", "150000 100000\\n", "50\\n"],
        "{read}"
    );
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.HostConfig.NanoCpus}} {{.HostConfig.PidsLimit}} {{.HostConfig.BlkioWeight}} {{.HostConfig.RestartPolicy.Name}}",
        "held",
    ]);
    assert_eq!(
        shown.stdout, "67108864 134217728 1500000000 50 300 always\n",
        "{shown}"
    );
    for (args, said) in [
        (
            &["--cpu-period", "50000", "held"][..],
            format!(
                "Error response from daemon: Cannot update container {id}: Conflicting options: CPU Period cannot be updated as NanoCPUs has already been set\n"
            ),
        ),
        (
            &["-m", "1m", "held"][..],
            "Error response from daemon: Minimum memory limit allowed is 6MB\n".to_string(),
        ),
        (
            &["--blkio-weight", "5", "held"][..],
            "Error response from daemon: Range of blkio weight is from 10 to 1000\n".to_string(),
        ),
        (
            &["--cpus", "1", "nope"][..],
            "Error response from daemon: No such container: nope\n".to_string(),
        ),
    ] {
        let mut all = vec!["update"];
        all.extend(args);
        let out = shards(&all);
        assert_eq!((out.status, out.stderr), (Some(1), said), "{args:?}");
    }
    let bare = shards(&["update", "held"]);
    assert_eq!(
        bare.stderr,
        "you must provide one or more flags when using this command\n"
    );
    assert_eq!(shards(&["rm", "-f", "held"]).status, Some(0));
    let _ = exit(&mut held);
    // On a stopped container the record changes, for its next start.
    assert_eq!(shards(&["create", "--name", "made", &image]).status, Some(0));
    assert_eq!(shards(&["update", "--cpus", "2", "made"]).stdout, "made\n");
    assert_eq!(
        shards(&["inspect", "-f", "{{.HostConfig.NanoCpus}}", "made"]).stdout,
        "2000000000\n"
    );
}

/// `ps --size` and `inspect --size`: the disk a microVM's writable layer uses, from its
/// guest while it runs and as its last run left it after, and with its image's root.
#[cfg(unix)]
#[test]
fn sizes_are_listed_as_docker_ps_and_inspect_list_them() {
    let Some((home, image)) = home("containers-sizes") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let mut held = start(&home, &image, &["--name", "sized", "-u", "0"], &["sleep"]);
    let wrote = shards(&["exec", "sized", "/bin/testguest", "fs", "write:/big=0123456789"]);
    assert_eq!(wrote.status, Some(0), "{wrote}");
    let sizes = |args: &[&str]| -> (i64, i64) {
        let mut all = vec!["inspect"];
        all.extend(args);
        all.extend(["-f", "{{.SizeRw}} {{.SizeRootFs}}", "sized"]);
        let out = shards(&all).stdout;
        let mut n = out.split_whitespace().map(|w| w.parse::<i64>().unwrap());
        (n.next().unwrap(), n.next().unwrap())
    };
    let (rw, root) = sizes(&["-s"]);
    assert!(rw > 0 && root > rw, "{rw} {root}");
    assert_eq!(
        shards(&["inspect", "-f", "{{.SizeRw}}", "sized"]).stdout,
        "<nil>\n",
        "only with --size"
    );
    let listed = shards(&["ps", "-s", "--format", "{{.Names}} {{.Size}}"]).stdout;
    assert!(
        listed.starts_with("sized ") && listed.contains(" (virtual "),
        "{listed}"
    );
    assert_eq!(shards(&["stop", "-t", "0", "sized"]).status, Some(0));
    let _ = exit(&mut held);
    assert_eq!(sizes(&["-s"]), (rw, root), "as its last run left it");
}

/// `import`, as `docker import`: a microVM's files exported, then imported as an image
/// of one layer, plain or compressed, from a file or stdin, its config from `--change`,
/// and run; dockerd's refusals.
#[cfg(unix)]
#[test]
fn import_makes_images_of_tarballs_as_docker_import_does() {
    let Some((home, image)) = home("containers-import") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    assert_eq!(shards(&["create", "--name", "source", &image]).status, Some(0));
    let tar = home.join("fs.tar");
    let exported = shards(&["export", "-o", tar.to_str().unwrap(), "source"]);
    assert_eq!(exported.status, Some(0), "{exported}");
    let made = shards(&[
        "import",
        "-m",
        "from source",
        "-c",
        "ENTRYPOINT [\"/bin/testguest\"]",
        "-c",
        "ENV IMPORTED=yes",
        tar.to_str().unwrap(),
        "imported:1",
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    assert!(
        made.stdout.starts_with("sha256:") && made.stdout.lines().count() == 1,
        "{made}"
    );
    let shown = shards(&[
        "image",
        "inspect",
        "-f",
        "{{json .Config.Entrypoint}} {{json .Config.Env}} {{.Comment}} {{len .RootFS.Layers}}",
        "imported:1",
    ]);
    assert_eq!(
        shown.stdout, "[\"/bin/testguest\"] [\"IMPORTED=yes\"] from source 1\n",
        "{shown}"
    );
    let ran = run_in(&home, "imported:1", &["--rm"], &["stat", "/proc/self/environ"]);
    assert!(ran.stdout.contains("IMPORTED=yes"), "{ran}");
    // gzip'd, from stdin: the layer as it came; its diff ID the tar's.
    let gz = home.join("fs.tar.gz");
    let zipped = Command::new("gzip").arg("-k").arg(&tar).status().unwrap();
    assert!(zipped.success());
    let piped = common::command()
        .args(["import", "-", "imported:2"])
        .env("SHARDS_HOME", &*home)
        .stdin(std::fs::File::open(&gz).unwrap())
        .output()
        .unwrap();
    assert!(
        piped.status.success(),
        "{}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let layers = |name: &str| shards(&["image", "inspect", "-f", "{{json .RootFS.Layers}}", name]).stdout;
    assert_eq!(layers("imported:1"), layers("imported:2"));
    assert_eq!(
        shards(&["image", "inspect", "-f", "{{.Comment}}", "imported:2"]).stdout,
        "Imported from -\n"
    );
    // Made a microVM, as a pull makes one, and published so to the local engine: here one
    // the test serves, which keeps what it is sent.
    // Removed however the test ends.
    struct Socket(std::path::PathBuf);
    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let socket = Socket(format!("/tmp/shards-engine-{}.sock", std::process::id()).into());
    let _ = std::fs::remove_file(&socket.0);
    let engine = std::os::unix::net::UnixListener::bind(&socket.0).unwrap();
    let (sent, served) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{BufRead as _, Read as _, Write as _};
        // Asked first whether it holds the microVM, which it does not.
        let (mut stream, mut reader, request) = loop {
            let (mut stream, _) = engine.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut request = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                request.push_str(&line);
                line.clear();
            }
            if request.starts_with("GET /images/shards.local%2Fimported%3A3/json ") {
                stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .unwrap();
                continue;
            }
            break (stream, reader, request);
        };
        let mut line = String::new();
        let mut body = Vec::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let n = usize::from_str_radix(line.trim(), 16).unwrap();
            let mut chunk = vec![0; n + 2];
            reader.read_exact(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            .unwrap();
        let _ = sent.send((request, body));
    });
    let published = common::command()
        .args([
            "import",
            "-c",
            "ENTRYPOINT [\"/bin/testguest\"]",
            "-c",
            "ENV IMPORTED=yes",
        ])
        .args([tar.to_str().unwrap(), "imported:3"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_LOCAL_STORE", "")
        .env("DOCKER_HOST", format!("unix://{}", socket.0.display()))
        .output()
        .unwrap();
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let (request, microvm) = served
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the import published nothing");
    drop(socket);
    assert!(request.starts_with("POST /images/load?quiet=1 "), "{request}");
    let artifact = String::from_utf8_lossy(&microvm);
    assert!(
        artifact.contains("application/vnd.shards.microvm.v1")
            && artifact.contains("shards.local/imported:3"),
        "not a microVM"
    );
    // A shards microVM imported is one already: run as the image it was made from.
    let vm = home.join("microvm.tar");
    std::fs::write(&vm, &microvm).unwrap();
    let back = shards(&["import", vm.to_str().unwrap(), "from-microvm:1"]);
    assert_eq!(back.status, Some(0), "{back}");
    let ran = run_in(
        &home,
        "from-microvm:1",
        &["--rm"],
        &["stat", "/proc/self/environ"],
    );
    assert!(ran.stdout.contains("IMPORTED=yes"), "{ran}");
    // An image archive: its images, each made a microVM, the one it holds named too.
    let saved = home.join("saved.tar");
    let out = shards(&["save", "-o", saved.to_str().unwrap(), "imported:1"]);
    assert_eq!(out.status, Some(0), "{out}");
    let from_save = shards(&["import", saved.to_str().unwrap(), "from-save:1"]);
    assert_eq!(from_save.status, Some(0), "{from_save}");
    let ran = run_in(&home, "from-save:1", &["--rm"], &["stat", "/proc/self/environ"]);
    assert!(ran.stdout.contains("IMPORTED=yes"), "{ran}");
    for (args, said) in [
        (
            &["import", "nope.tar"][..],
            "open nope.tar: no such file or directory\n",
        ),
        (
            &["import", "-c", "RUN x", tar.to_str().unwrap()][..],
            "Error response from daemon: RUN is not a valid change command\n",
        ),
        (
            &["import", tar.to_str().unwrap(), "BAD:REF:x"][..],
            "invalid reference format: repository name (library/BAD) must be lowercase\n",
        ),
    ] {
        let out = shards(args);
        assert_eq!((out.status, out.stderr.as_str()), (Some(1), said), "{args:?}");
    }
}

/// A run that ends while its container is inspected: the end holds the run's inbox while
/// it takes the daemon's runs, and inspect, which reads the run's execs, took the runs and
/// then the inbox, so that each waited on the other for good (seen in a full suite,
/// 2026-10-05). Inspected over and over while runs end, every step keeps its deadline.
#[test]
fn inspecting_a_run_as_it_ends_never_deadlocks() {
    let Some((home, image)) = home("containers-inspect-ending") else {
        return;
    };
    let current = std::sync::Mutex::new(String::new());
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let inspectors: Vec<_> = (0..16)
            .map(|_| {
                s.spawn(|| {
                    let mut asked = 0;
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        let name = current.lock().unwrap().clone();
                        if name.is_empty() {
                            std::thread::yield_now();
                            continue;
                        }
                        // Asked before the container is made, it is not there yet: what fails is
                        // only a step that never ends.
                        let out = shards_in(&home, &["inspect", "-f", "{{.State.Status}}", &name]);
                        assert!(
                            out.status == Some(0) || out.stderr.contains("no such object"),
                            "{out}"
                        );
                        asked += usize::from(out.status == Some(0));
                    }
                    asked
                })
            })
            .collect();
        // However this loop ends, the inspectors stop: a failed step fails the test, and
        // never leaves them spinning.
        struct Stop<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let stop = Stop(&done);
        for i in 0..100 {
            let name = format!("ending-{i}");
            *current.lock().unwrap() = name.clone();
            let out = run_in(&home, &image, &["-d", "--name", &name], &["exit", "0"]);
            assert_eq!(out.status, Some(0), "{out}");
            let waited = shards_in(&home, &["wait", &name]);
            assert_eq!(waited.status, Some(0), "{waited}");
        }
        drop(stop);
        let asked: usize = inspectors.into_iter().map(|i| i.join().unwrap()).sum();
        assert!(asked > 40, "inspected only {asked} times");
    });
}

/// `--security-opt` (D42), as Docker's: the default profile's filter on the workload and
/// its execs, none for `unconfined` or a privileged container (but one it names; here
/// `builtin` too, which dockerd fails to read), a profile's file read by the CLI and
/// kept in SecurityOpt, `no-new-privileges`, `systempaths=unconfined` leaving the paths
/// unmasked, and refusals in dockerd's words: of an option as the container is made, of
/// a profile as it starts, the container kept, created, with State.Error.
#[test]
fn security_opt_confines_as_docker_does() {
    let Some((home, image)) = home("containers-security-opt") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let field = |out: &str, key: &str| -> String {
        out.split("\\n")
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let status = |opts: &[&str]| {
        let mut o = vec!["--rm"];
        o.extend_from_slice(opts);
        let ran = run_in(&home, &image, &o, &["stat", "/proc/self/status"]);
        assert_eq!(ran.status, Some(0), "{opts:?}: {ran}");
        (field(&ran.stdout, "Seccomp:"), field(&ran.stdout, "NoNewPrivs:"))
    };
    assert_eq!(status(&[]), ("2".into(), "0".into()), "the default profile");
    assert_eq!(status(&["--security-opt", "seccomp=unconfined"]).0, "0");
    assert_eq!(status(&["--privileged"]).0, "0");
    assert_eq!(
        status(&["--privileged", "--security-opt", "seccomp=builtin"]).0,
        "2"
    );
    assert_eq!(
        status(&["--security-opt", "no-new-privileges"]),
        ("2".into(), "1".into())
    );
    assert_eq!(
        status(&["--security-opt", "no-new-privileges:true", "-u", "1000"]).1,
        "1"
    );
    // A profile's file: mkdir refused, in the workload and in an exec of it.
    let profile = home.join("deny-mkdir.json");
    std::fs::write(
        &profile,
        r#"{ "defaultAction": "SCMP_ACT_ALLOW",
             "syscalls": [ { "names": ["mkdir", "mkdirat"], "action": "SCMP_ACT_ERRNO" } ] }"#,
    )
    .unwrap();
    let opt = format!("seccomp={}", profile.display());
    let refused = run_in(
        &home,
        &image,
        &["--rm", "--security-opt", &opt],
        &["fs", "mkdir:/made"],
    );
    assert_ne!(refused.status, Some(0), "{refused}");
    assert!(refused.stderr.contains("Operation not permitted"), "{refused}");
    let made = run_in(
        &home,
        &image,
        &["-d", "--name", "confined", "--security-opt", &opt],
        &["sleep"],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let exec = shards(&["exec", "confined", "/bin/testguest", "fs", "mkdir:/made"]);
    assert!(exec.stderr.contains("Operation not permitted"), "{exec}");
    let kept = shards(&["inspect", "-f", "{{json .HostConfig.SecurityOpt}}", "confined"]);
    assert_eq!(
        kept.stdout,
        "[\"seccomp={\\\"defaultAction\\\":\\\"SCMP_ACT_ALLOW\\\",\\\"syscalls\\\":[{\\\"names\\\":[\\\"mkdir\\\",\\\"mkdirat\\\"],\\\"action\\\":\\\"SCMP_ACT_ERRNO\\\"}]}\"]\n"
    );
    assert_eq!(shards(&["rm", "-f", "confined"]).status, Some(0));
    // systempaths=unconfined: /proc/kcore is the kernel's, not /dev/null over it.
    let kcore = |opts: &[&str]| {
        let mut o = vec!["--rm"];
        o.extend_from_slice(opts);
        run_in(&home, &image, &o, &["stat", "/proc/kcore"]).stdout
    };
    assert!(kcore(&[]).starts_with("/proc/kcore other"), "{}", kcore(&[]));
    assert!(kcore(&["--security-opt", "systempaths=unconfined"]).starts_with("/proc/kcore file"));
    let made = shards(&[
        "create",
        "--security-opt",
        "systempaths=unconfined",
        "--security-opt",
        "label=disable",
        &image,
    ]);
    let id = made.stdout.trim().to_string();
    let shown = shards(&[
        "inspect",
        "-f",
        "{{json .HostConfig.SecurityOpt}} {{json .HostConfig.MaskedPaths}} {{json .HostConfig.ReadonlyPaths}}",
        &id,
    ]);
    assert_eq!(shown.stdout, "[\"label=disable\"] [] []\n");
    assert_eq!(shards(&["rm", &id]).status, Some(0));
    // Refusals: the CLI's, dockerd's as it makes the container, and as it starts it.
    for (opt, said) in [
        ("bogus", "invalid --security-opt: \"bogus\""),
        (
            "no-new-privileges=yes",
            "Error response from daemon: invalid --security-opt 2: \"no-new-privileges=yes\"",
        ),
        (
            "bogus=1",
            "Error response from daemon: invalid --security-opt 2: \"bogus=1\"",
        ),
    ] {
        let out = shards(&["run", "--rm", "--security-opt", opt, &image, "exit", "0"]);
        assert_eq!(out.status, Some(125), "{out}");
        assert!(out.stderr.contains(said), "{opt}: {out}");
    }
    let bad = home.join("bad-action.json");
    std::fs::write(&bad, r#"{"defaultAction":"SCMP_ACT_NOPE"}"#).unwrap();
    let opt = format!("seccomp={}", bad.display());
    let made = shards(&[
        "create",
        "--name",
        "bad",
        "--security-opt",
        &opt,
        &image,
        "exit",
        "0",
    ]);
    assert_eq!(made.status, Some(0), "made as dockerd makes it: {made}");
    let started = shards(&["start", "bad"]);
    assert_eq!(started.status, Some(1), "{started}");
    assert!(
        started
            .stderr
            .contains("string SCMP_ACT_NOPE is not a valid action for seccomp"),
        "{started}"
    );
    let state = shards(&[
        "inspect",
        "-f",
        "{{.State.Status}} {{.State.ExitCode}} {{.State.Error}}",
        "bad",
    ]);
    assert_eq!(
        state.stdout,
        "created 128 string SCMP_ACT_NOPE is not a valid action for seccomp\n"
    );
    let ran = shards(&["run", "--rm", "--security-opt", &opt, &image, "exit", "0"]);
    assert_eq!(ran.status, Some(125), "{ran}");
}

/// A workload starts with Docker's resource limits (Docker Desktop 29.3.1's, from its
/// runtime's: open files 1048576, processes and locked memory unlimited), not the guest
/// kernel's own init's; `--ulimit` sets its own over them, in an exec too.
#[test]
fn workloads_start_with_dockers_limits() {
    let Some((home, image)) = home("containers-limits") else {
        return;
    };
    let limit = |out: &str, name: &str| -> String {
        out.split("\\n")
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    let ran = run_in(&home, &image, &["--rm"], &["stat", "/proc/self/limits"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    assert_eq!(limit(&ran.stdout, "Max open files"), "1048576 1048576", "{ran}");
    assert_eq!(limit(&ran.stdout, "Max processes"), "unlimited unlimited");
    assert_eq!(limit(&ran.stdout, "Max locked memory"), "unlimited unlimited");
    let made = run_in(
        &home,
        &image,
        &["-d", "--name", "limited", "--ulimit", "nofile=1024:2048"],
        &["sleep"],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let exec = shards_in(
        &home,
        &["exec", "limited", "/bin/testguest", "stat", "/proc/self/limits"],
    );
    assert_eq!(limit(&exec.stdout, "Max open files"), "1024 2048", "{exec}");
    assert_eq!(limit(&exec.stdout, "Max processes"), "unlimited unlimited");
    assert_eq!(shards_in(&home, &["rm", "-f", "limited"]).status, Some(0));
}

/// `stats` reads each running container's guest as dockerd reads a container's cgroup:
/// its CPU over the second between two samples, memory less inactive file pages against
/// its limit, its processes, and its interfaces' and block devices' bytes, laid out as
/// docker/cli lays them out, `--format` included; a stopped one's zeros under `-a`.
#[test]
fn stats_read_the_guest_as_docker_reads_a_container() {
    let Some((home, image)) = home("containers-stats-guest") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = run_in(
        &home,
        &image,
        &["-d", "--name", "busy", "--memory", "64m"],
        &["spin"],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let shown = shards(&["stats", "--no-stream", "--format", "{{json .}}", "busy"]);
    assert_eq!(shown.status, Some(0), "{shown}");
    let v: serde_json::Value = serde_json::from_str(shown.stdout.trim()).unwrap();
    assert_eq!(v["Container"], "busy");
    assert_eq!(v["Name"], "busy");
    let cpu: f64 = v["CPUPerc"]
        .as_str()
        .unwrap()
        .trim_end_matches('%')
        .parse()
        .unwrap();
    // What the host gives its vCPU, measured from the guest: on a busy host less than a
    // CPU (14.44% on a shared CI runner, 2026-10-07), so it is set against a microVM
    // asleep, which uses next to none.
    let asleep = run_in(&home, &image, &["-d", "--name", "asleep"], &["sleep"]);
    assert_eq!(asleep.status, Some(0), "{asleep}");
    let still = shards(&["stats", "--no-stream", "--format", "{{.CPUPerc}}", "asleep"]);
    let sleeping: f64 = still.stdout.trim().trim_end_matches('%').parse().unwrap();
    assert!(
        cpu > 10.0 * sleeping.max(1.0),
        "a spinning workload's CPU, against {sleeping}% asleep: {shown}"
    );
    let mem = v["MemUsage"].as_str().unwrap();
    assert!(mem.ends_with(" / 64MiB"), "its limit: {shown}");
    assert_eq!(v["PIDs"], "1", "{shown}");
    let table = shards(&["stats", "--no-stream", "busy"]);
    assert!(
        table
            .stdout
            .starts_with("CONTAINER ID   NAME      CPU %     MEM USAGE / LIMIT   MEM %     NET I/O"),
        "{table}"
    );
    assert_eq!(shards(&["create", "--name", "idle", &image]).status, Some(0));
    let all = shards(&[
        "stats",
        "--no-stream",
        "-a",
        "--format",
        "{{.Name}} {{.CPUPerc}} {{.MemUsage}} {{.PIDs}}",
    ]);
    assert!(all.stdout.lines().any(|l| l == "idle 0.00% 0B / 0B 0"), "{all}");
    assert_eq!(shards(&["rm", "-f", "busy", "idle", "asleep"]).status, Some(0));
}

/// D44: a container reaches Docker's devices and those it is given, as dockerd and runc
/// confine one (moby's default rules, runc's eBPF device filter), its execs too; `--device`
/// takes the VM's devices, as Docker's takes its host's; every refusal is Docker's (probed
/// on Docker Engine 29.3.1); `a` rules allow what they say.
#[test]
fn devices_are_given_and_confined_as_dockers_are() {
    let Some((home, image)) = home("containers-devices") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let fs = |opts: &[&str], ops: &[&str]| {
        let mut o = vec!["--rm"];
        o.extend_from_slice(opts);
        let mut a = vec!["fs"];
        a.extend_from_slice(ops);
        run_in(&home, &image, &o, &a)
    };
    // Any node made, but the image's disk (259:0) not opened; Docker's own devices are.
    let root = ["-u", "0"];
    let r = fs(&root, &["mknod:b:259:0:/disk", "open:r:/disk"]);
    assert!(r.stderr.contains("open:r:/disk: Operation not permitted"), "{r}");
    let r = fs(
        &[],
        &[
            "open:w:/dev/null",
            "open:r:/dev/urandom",
            "open:w:/dev/full",
            "open:w:/dev/zero",
        ],
    );
    assert_eq!(r.status, Some(0), "{r}");
    let r = fs(
        &["-u", "0", "--device-cgroup-rule", "b 259:0 r"],
        &["mknod:b:259:0:/disk", "open:r:/disk"],
    );
    assert_eq!(r.status, Some(0), "{r}");
    // Reading every device is that alone, where runc's reading of `a` gives writing too.
    let r = fs(
        &["-u", "0", "--device-cgroup-rule", "a *:* r"],
        &["mknod:b:7:0:/loop", "open:r:/loop", "open:w:/loop"],
    );
    assert!(r.stderr.contains("open:w:/loop: Operation not permitted"), "{r}");
    // A device of the VM's, at another path, with a host's mode and the access given: the
    // image's user opens it.
    let r = fs(
        &["--device", "/dev/fuse:/dev/x:r"],
        &["dev:/dev/x", "open:r:/dev/x", "open:w:/dev/x"],
    );
    assert!(r.stdout.contains("/dev/x c 10:229 666"), "{r}");
    assert!(r.stderr.contains("open:w:/dev/x: Operation not permitted"), "{r}");
    // A directory's devices.
    let r = fs(
        &["--device", "/dev/net"],
        &["dev:/dev/net/tun", "open:w:/dev/net/tun"],
    );
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("/dev/net/tun c 10:200 666"), "{r}");
    // Privileged: every device, as the VM has it.
    assert_eq!(
        fs(&["-u", "0", "--privileged"], &["open:r:/dev/pmem0"]).status,
        Some(0)
    );
    // An exec is confined as its container is.
    let made = run_in(&home, &image, &["-d", "--name", "confined"], &["sleep"]);
    assert_eq!(made.status, Some(0), "{made}");
    let exec = shards(&[
        "exec",
        "-u",
        "0",
        "confined",
        "/bin/testguest",
        "fs",
        "mknod:b:259:0:/disk",
        "open:r:/disk",
    ]);
    assert!(
        exec.stderr.contains("open:r:/disk: Operation not permitted"),
        "{exec}"
    );
    assert_eq!(shards(&["rm", "-f", "confined"]).status, Some(0));
    // The CLI's refusals.
    for (opts, words) in [
        (["--device", "/dev/fuse:/dev/x:rwx"], "bad mode specified: rwx"),
        (["--device", "rel"], "rel is not an absolute path"),
        (["--device", "/a:/b:r:x"], "bad format for path: /a:/b:r:x"),
        (
            ["--device-cgroup-rule", "c 1:3"],
            "invalid argument \"c 1:3\" for \"--device-cgroup-rule\" flag: invalid device cgroup format 'c 1:3'",
        ),
    ] {
        let r = run_in(&home, &image, &opts, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{opts:?}: {r}");
        assert!(r.stderr.contains(words), "{opts:?}: {r}");
    }
    // dockerd's, as the container starts: kept, created, 128, and its error.
    let missing = "error gathering device information while adding custom device \"/dev/nope\": no such file or directory";
    let r = run_in(&home, &image, &["--rm", "--device", "/dev/nope"], &["exit", "0"]);
    assert_eq!(r.status, Some(127), "{r}");
    assert!(r.stderr.contains(missing), "{r}");
    let made = shards(&[
        "create",
        "--pull",
        "never",
        "--name",
        "nodev",
        "--device",
        "/dev/nope",
        &image,
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let started = shards(&["start", "nodev"]);
    assert_eq!(started.status, Some(1), "{started}");
    assert_eq!(
        started.stderr,
        format!("Error response from daemon: {missing}\nfailed to start containers: nodev\n")
    );
    let state = shards(&[
        "inspect",
        "-f",
        "{{.State.Status}} {{.State.ExitCode}} {{.State.Error}}",
        "nodev",
    ]);
    assert_eq!(state.stdout, format!("created 128 {missing}\n"));
    let r = run_in(
        &home,
        &image,
        &["--rm", "--device", "vendor.com/gpu=0"],
        &["exit", "0"],
    );
    assert!(
        r.stderr
            .contains("CDI device injection failed: unresolvable CDI devices vendor.com/gpu=0"),
        "{r}"
    );
    // What inspect says of them, as Docker's does.
    let fields = "{{json .HostConfig.Devices}} {{json .HostConfig.DeviceCgroupRules}} {{json .HostConfig.DeviceRequests}}";
    let made = shards(&[
        "create",
        "--pull",
        "never",
        "--name",
        "devices",
        "--device",
        "/dev/fuse:/dev/x:r",
        "--device",
        "vendor.com/gpu=0",
        "--device-cgroup-rule",
        "c 1:3 r",
        &image,
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    assert_eq!(
        shards(&["inspect", "-f", fields, "devices"]).stdout,
        "[{\"PathOnHost\":\"/dev/fuse\",\"PathInContainer\":\"/dev/x\",\"CgroupPermissions\":\"r\"}] [\"c 1:3 r\"] \
         [{\"Driver\":\"cdi\",\"Count\":0,\"DeviceIDs\":[\"vendor.com/gpu=0\"],\"Capabilities\":null,\"Options\":null}]\n"
    );
    let made = shards(&["create", "--pull", "never", "--name", "plain", &image]);
    assert_eq!(made.status, Some(0), "{made}");
    assert_eq!(
        shards(&["inspect", "-f", fields, "plain"]).stdout,
        "[] null null\n"
    );
}

/// D44: a container's block I/O limits on the VM's devices, as runc writes them (fs2
/// setIo) and the kernel keeps them: a read of the image's disk held to its rate; every
/// refusal Docker's (probed on Docker Engine 29.3.1), and inspect's lists.
#[test]
fn block_io_limits_hold_as_dockers_do() {
    let Some((home, image)) = home("containers-block-io") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let limits = [
        "--device-read-bps",
        "/dev/pmem0:1mb",
        "--device-write-bps",
        "/dev/loop0:2k",
        "--device-read-iops",
        "/dev/loop1:7",
        "--device-write-iops",
        "/dev/loop0:100",
    ];
    let mut opts = vec!["--rm"];
    opts.extend_from_slice(&limits);
    let r = run_in(&home, &image, &opts, &["fs", "print:/sys/fs/cgroup/io.max"]);
    assert_eq!(r.status, Some(0), "{r}");
    let mut lines: Vec<&str> = r.stdout.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "259:0 rbps=1048576 wbps=max riops=max wiops=max",
            "7:0 rbps=max wbps=2048 riops=max wiops=100",
            "7:1 rbps=max wbps=max riops=7 wiops=max",
        ]
    );
    // A MiB of the disk (of two) at half a MiB a second: no less than a second, where
    // unlimited it takes milliseconds.
    let read = |extra: &[&str]| {
        let mut o = vec!["--rm", "-u", "0", "--device", "/dev/pmem0"];
        o.extend_from_slice(extra);
        let started = std::time::Instant::now();
        let r = run_in(&home, &image, &o, &["fs", "readn:1048576:/dev/pmem0"]);
        assert_eq!(r.status, Some(0), "{r}");
        started.elapsed()
    };
    let held = read(&["--device-read-bps", "/dev/pmem0:512kb"]);
    assert!(held >= std::time::Duration::from_secs(1), "{held:?}");
    let free = read(&[]);
    assert!(free < held, "{free:?} {held:?}");
    // dockerd's and runc's refusals, as the container starts.
    for (opts, words, status) in [
        (
            ["--device-read-bps", "/dev/nope:1mb"],
            "stat /dev/nope: no such file or directory",
            127,
        ),
        (
            ["--device-read-iops", "/dev/pmem0:0"],
            "error setting cgroup config for procHooks process: failed to write \"259:0 riops=0\": write /sys/fs/cgroup/io.max: numerical result out of range",
            125,
        ),
        (
            ["--blkio-weight-device", "/dev/pmem0:300"],
            "error setting cgroup config for procHooks process: setting device weight \"259:0 300\": write /sys/fs/cgroup/io.bfq.weight: operation not supported",
            125,
        ),
    ] {
        let r = run_in(&home, &image, &opts, &["exit", "0"]);
        assert_eq!(r.status, Some(status), "{opts:?}: {r}");
        assert!(r.stderr.contains(words), "{opts:?}: {r}");
    }
    // The CLI's.
    for (opts, words) in [
        (
            ["--device-read-bps", "/dev/sda"],
            "invalid argument \"/dev/sda\" for \"--device-read-bps\" flag: bad format: /dev/sda",
        ),
        (
            ["--device-read-bps", "rel:1mb"],
            "bad format for device path: rel:1mb",
        ),
        (
            ["--device-read-bps", "/dev/sda:1xb"],
            "invalid rate for device: /dev/sda:1xb. The correct format is <device-path>:<number>[<unit>]. Number must be a positive integer. Unit is optional and can be kb, mb, or gb",
        ),
        (
            ["--device-write-iops", "/dev/sda:1.5"],
            "invalid rate for device: /dev/sda:1.5. The correct format is <device-path>:<number>. Number must be a positive integer",
        ),
        (
            ["--blkio-weight-device", "/dev/sda:5"],
            "invalid weight for device: /dev/sda:5",
        ),
        (
            ["--blkio-weight-device", "/dev/sda:1001"],
            "invalid weight for device: /dev/sda:1001",
        ),
    ] {
        let r = run_in(&home, &image, &opts, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{opts:?}: {r}");
        assert!(r.stderr.contains(words), "{opts:?}: {r}");
    }
    let made = shards(&[
        "create",
        "--pull",
        "never",
        "--name",
        "limited",
        "--device-read-bps",
        "/dev/pmem0:1.5mb",
        "--device-write-iops",
        "/dev/pmem0:100",
        "--blkio-weight-device",
        "/dev/pmem0:300",
        &image,
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let fields = "{{json .HostConfig.BlkioWeightDevice}} {{json .HostConfig.BlkioDeviceReadBps}} {{json .HostConfig.BlkioDeviceWriteBps}} {{json .HostConfig.BlkioDeviceReadIOps}} {{json .HostConfig.BlkioDeviceWriteIOps}}";
    assert_eq!(
        shards(&["inspect", "-f", fields, "limited"]).stdout,
        "[{\"Path\":\"/dev/pmem0\",\"Weight\":300}] [{\"Path\":\"/dev/pmem0\",\"Rate\":1572864}] [] [] [{\"Path\":\"/dev/pmem0\",\"Rate\":100}]\n"
    );
    let made = shards(&["create", "--pull", "never", "--name", "plain", &image]);
    assert_eq!(made.status, Some(0), "{made}");
    assert_eq!(
        shards(&["inspect", "-f", fields, "plain"]).stdout,
        "[] [] [] [] []\n"
    );
}

/// `shards attach`, as `docker attach` attaches (probed on Docker Engine 29.3.1): to a
/// running container's command, its stdin where the container reads one, its output from
/// now, until the command ends, whose status is the client's; refused as docker/cli
/// refuses it. Its stdin's end leaves a detached container's open and its output still
/// coming, where Docker's attach ends with its stdin and loses what follows.
#[test]
fn attach_joins_a_running_container_as_docker_attach_does() {
    use std::io::Write as _;
    let Some((home, image)) = home("containers-attach") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = run_in(&home, &image, &["-d", "-i", "--name", "echo"], &["cat"]);
    assert_eq!(made.status, Some(0), "{made}");
    let attach = || {
        common::command()
            .args(["attach", "echo"])
            .env("SHARDS_HOME", &*home)
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    // What one attached client writes, it reads back; its stdin's end ends neither the
    // command's stdin nor its output to it.
    let mut first = attach();
    let mut first_out = BufReader::new(first.stdout.take().unwrap());
    first.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let mut line = String::new();
    first_out.read_line(&mut line).unwrap();
    assert_eq!(line, "hello\n");
    let mut second = attach();
    let mut second_out = BufReader::new(second.stdout.take().unwrap());
    let mut second_in = second.stdin.take().unwrap();
    second_in.write_all(b"again\n").unwrap();
    for out in [&mut first_out, &mut second_out] {
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        assert_eq!(line, "again\n");
    }
    // Both end with the command, with its status: SIGTERM's.
    assert_eq!(shards(&["stop", "echo"]).status, Some(0));
    assert_eq!(exit(&mut first), Some(143));
    assert_eq!(exit(&mut second), Some(143));
    drop(second_in);
    // docker/cli's refusals.
    let stopped = shards(&["attach", "echo"]);
    assert_eq!(
        (stopped.status, stopped.stderr.as_str()),
        (Some(1), "cannot attach to a stopped container, start it first\n")
    );
    let missing = shards(&["attach", "nope"]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (Some(1), "Error response from daemon: No such container: nope\n")
    );
    let made = run_in(&home, &image, &["-d", "--name", "held"], &["sleep"]);
    assert_eq!(made.status, Some(0), "{made}");
    assert_eq!(shards(&["pause", "held"]).status, Some(0));
    let paused = shards(&["attach", "held"]);
    assert_eq!(
        (paused.status, paused.stderr.as_str()),
        (Some(1), "cannot attach to a paused container, unpause it first\n")
    );
    assert_eq!(shards(&["rm", "-f", "held", "echo"]).status, Some(0));
    let made = run_in(&home, &image, &["-d", "-t", "--name", "term"], &["sleep"]);
    assert_eq!(made.status, Some(0), "{made}");
    let piped = shards(&["attach", "term"]);
    assert_eq!(
        (piped.status, piped.stderr.as_str()),
        (
            Some(1),
            "cannot attach stdin to a TTY-enabled container because stdin is not a terminal\n"
        )
    );
    assert_eq!(shards(&["rm", "-f", "term"]).status, Some(0));
}

/// D99: microVMs on a network with IPv6 have an address of its IPv6 subnet each, the
/// lowest free (the subnet's first address and the gateway kept out), or the one `--ip6`
/// asks for: they reach one another at it, the server sees the client's own, their names
/// resolve to both versions, `/etc/hosts` names each at both, and inspect says it as
/// dockerd does.
#[test]
fn microvms_on_an_ipv6_network_reach_one_another() {
    let Some((home, image)) = home("containers-ipv6") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = shards(&[
        "network",
        "create",
        "--ipv6",
        "--subnet",
        "10.78.0.0/24",
        "--subnet",
        "fd78::/64",
        "lan6",
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let mut srv = start(
        &home,
        &image,
        &["--name", "srv6", "--network", "lan6", "--ip6", "fd78::50"],
        &["serve", "7000", "3"],
    );
    // By its IPv6 address: the client's own, the lowest free, is what the server sees.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "lan6"],
        &["ask", "[fd78::50]:7000"],
    );
    assert_eq!(r.status, Some(0), "{r}");
    assert_eq!(r.stdout, "ask from fd78::2\n", "{r}");
    // Its name has both versions' addresses.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "lan6"],
        &["resolve", "srv6"],
    );
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("resolve fd78::50\n"), "{r}");
    assert!(r.stdout.contains("resolve 10.78.0."), "{r}");
    // And by name, whichever version the resolver puts first.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "lan6"],
        &["ask", "srv6:7000"],
    );
    assert_eq!(r.status, Some(0), "{r}");
    // Its own hosts file names it at both, its IPv6 address after its IPv4 one.
    let r = run_in(
        &home,
        &image,
        &[
            "--rm",
            "--network",
            "lan6",
            "--hostname",
            "six",
            "--ip6",
            "fd78::60",
        ],
        &["fs", "print:/etc/hosts"],
    );
    assert!(r.stdout.ends_with("\tsix\nfd78::60\tsix\n"), "{r}");
    let endpoint = shards(&[
        "inspect",
        "-f",
        "{{with .NetworkSettings.Networks.lan6}}{{.GlobalIPv6Address}} {{.IPv6Gateway}} {{.GlobalIPv6PrefixLen}}{{end}}",
        "srv6",
    ]);
    assert_eq!(endpoint.stdout, "fd78::50 fd78::1 64\n", "{endpoint}");
    let members = shards(&[
        "network",
        "inspect",
        "-f",
        "{{range .Containers}}{{.Name}} {{.IPv6Address}}{{end}}",
        "lan6",
    ]);
    assert_eq!(members.stdout, "srv6 fd78::50/64\n", "{members}");
    // An address taken is refused in dockerd's words.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "lan6", "--ip6", "fd78::50"],
        &["exit", "0"],
    );
    assert!(
        r.stderr
            .contains("failed to set up container networking: Address already in use"),
        "{r}"
    );
    assert_eq!(shards(&["rm", "-f", "srv6"]).status, Some(0));
    let _ = srv.kill();
    let _ = srv.wait();
    assert_eq!(shards(&["network", "rm", "lan6"]).stdout, "lan6\n");
}

/// D46: microVMs on a user network reach one another, by address and by every name
/// Docker's embedded DNS answers (name, alias, short ID, host name, any case), at
/// Docker's 127.0.0.11; the server sees each by its own address; a microVM off the network
/// reaches none of them; and `shards network` creates, lists, inspects, refuses and
/// removes networks in Docker's words (probed on Docker Engine 29.3.1).
#[test]
fn microvms_on_a_network_reach_one_another_by_name() {
    let Some((home, image)) = home("containers-networks") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = shards(&[
        "network",
        "create",
        "--subnet",
        "10.77.0.0/24",
        "--gateway",
        "10.77.0.1",
        "lan",
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    assert_eq!(made.stdout.trim().len(), 64, "{made}");
    // A server, by name, alias and address; it is said each client's own address.
    let mut srv = start(
        &home,
        &image,
        &[
            "--name",
            "srv",
            "--hostname",
            "srvhost",
            "--network",
            "lan",
            "--network-alias",
            "web",
            "--ip",
            "10.77.0.50",
        ],
        &["serve", "7000", "6"],
    );
    let id = shards(&["inspect", "-f", "{{.Id}}", "srv"])
        .stdout
        .trim()
        .get(..12)
        .unwrap()
        .to_string();
    for to in [
        "srv:7000",
        "WEB:7000",
        "srvhost:7000",
        "10.77.0.50:7000",
        &format!("{id}:7000"),
    ] {
        let r = run_in(&home, &image, &["--rm", "--network", "lan"], &["ask", to]);
        assert_eq!(r.status, Some(0), "{to}: {r}");
        assert!(r.stdout.starts_with("ask from 10.77.0."), "{to}: {r}");
        assert!(!r.stdout.contains("10.77.0.50\n"), "{to}: {r}");
    }
    // Its own name too, and Docker's resolver address in its resolv.conf.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "lan", "--name", "me"],
        &["fs", "print:/etc/resolv.conf"],
    );
    assert_eq!(r.stdout, "nameserver 127.0.0.11\noptions ndots:0\n", "{r}");
    // Off the network: no reach.
    let r = run_in(&home, &image, &["--rm"], &["ask", "10.77.0.50:7000"]);
    assert_ne!(r.status, Some(0), "{r}");
    // What inspect says of both.
    let members = shards(&[
        "network",
        "inspect",
        "-f",
        "{{range .Containers}}{{.Name}} {{.IPv4Address}}{{end}}",
        "lan",
    ]);
    assert_eq!(members.stdout, "srv 10.77.0.50/24\n", "{members}");
    let endpoint = shards(&[
        "inspect",
        "-f",
        "{{with .NetworkSettings.Networks.lan}}{{.IPAddress}} {{.Gateway}} {{.IPPrefixLen}} {{.Aliases}} {{.DNSNames}}{{end}}",
        "srv",
    ]);
    assert_eq!(
        endpoint.stdout,
        format!("10.77.0.50 10.77.0.1 24 [web] [srv web {id} srvhost]\n"),
        "{endpoint}"
    );
    // dockerd's refusals: an address taken, or out of the subnet; a network in use.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "lan", "--ip", "10.77.0.50"],
        &["exit", "0"],
    );
    assert!(
        r.stderr
            .contains("failed to set up container networking: Address already in use"),
        "{r}"
    );
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "lan", "--ip", "10.9.9.9"],
        &["exit", "0"],
    );
    assert!(
        r.stderr.contains("invalid config for network lan: invalid endpoint settings:\nno configured subnet contains IP address 10.9.9.9"),
        "{r}"
    );
    let rm = shards(&["network", "rm", "lan"]);
    assert_eq!(rm.status, Some(1), "{rm}");
    assert!(rm.stderr.starts_with("Error response from daemon: error while removing network: network lan has active endpoints (name:\"srv\" id:\""), "{rm}");
    assert!(rm.stderr.ends_with("\")\nexit status 1\n"), "{rm}");
    assert_eq!(shards(&["rm", "-f", "srv"]).status, Some(0));
    let _ = srv.kill();
    let _ = srv.wait();
    // Docker's table, and its refusals.
    let ls = shards(&["network", "ls", "--format", "{{.Name}} {{.Driver}} {{.Scope}}"]);
    assert_eq!(
        ls.stdout, "bridge bridge local\nhost host local\nlan bridge local\nnone null local\n",
        "{ls}"
    );
    for (args, said) in [
        (
            &["network", "create", "lan"][..],
            "Error response from daemon: network with name lan already exists\n",
        ),
        (
            &["network", "create", "bridge"],
            "Error response from daemon: operation is not permitted on predefined bridge network \n",
        ),
        (
            &["network", "create", "--subnet", "10.77.0.0/24", "other"],
            "Error response from daemon: invalid pool request: Pool overlaps with other one on this address space\n",
        ),
        (
            &["network", "create", "--gateway", "1.2.3.4", "other"],
            "every ip-range or gateway must have a corresponding subnet\n",
        ),
        (
            &["network", "rm", "bridge"],
            "Error response from daemon: bridge is a pre-defined network and cannot be removed\nexit status 1\n",
        ),
    ] {
        let r = shards(args);
        assert_eq!((r.status, r.stderr.as_str()), (Some(1), said), "{args:?}");
    }
    assert_eq!(shards(&["network", "rm", "lan"]).stdout, "lan\n");
    let made = shards(&["network", "create", "spare"]);
    assert_eq!(made.status, Some(0), "{made}");
    assert_eq!(
        shards(&["network", "prune", "-f"]).stdout,
        "Deleted Networks:\nspare\n\n"
    );
}

/// An image's config as Docker 29.3.1 reads it (shards_image::config, held to Go by its
/// testdata/image-config.json): a key matches its field exactly or else folded, the last of
/// those that match winning, so `"User":"root","user":"app"` runs as app, as Docker runs it.
/// One Go reads into no DockerOCIImage loads, as Docker loads it, and its run, create, tag
/// and inspect are refused in Docker's words; its history is read as Docker's history reads
/// its own part of it (each answer measured from Docker 29.3.1, its name ours).
#[test]
fn configs_are_read_as_docker_reads_them() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("containers-config-read");
    let shards = |args: &[&str]| shards_in(&home, args);
    let (_, blobs) = common::test_image_with(Some(b"config-read"));
    let (config, layer) = (String::from_utf8(blobs[0].clone()).unwrap(), &blobs[1]);
    assert!(config.contains(r#""User":"app""#), "{config}");
    let load = |name: &str, config: &str| {
        let manifest =
            format!(r#"[{{"Config":"cfg.json","RepoTags":["{name}"],"Layers":["abc/layer.tar"]}}]"#);
        let archive = common::tar(&[
            ("cfg.json", 0o644, 0, Some(config.as_bytes())),
            ("abc", 0o755, 0, None),
            ("abc/layer.tar", 0o644, 0, Some(layer.as_slice())),
            ("manifest.json", 0o644, 0, Some(manifest.as_bytes())),
        ]);
        let at = home.join(format!("{}.tar", name.replace(':', "-")));
        std::fs::write(&at, archive).unwrap();
        let loaded = shards(&["load", "-i", at.to_str().unwrap()]);
        assert_eq!(
            (loaded.status, loaded.stdout.as_str()),
            (Some(0), format!("Loaded image: {name}\n").as_str()),
            "{loaded}"
        );
    };
    load(
        "folded:1",
        &config.replacen(r#""User":"app""#, r#""User":"root","user":"app""#, 1),
    );
    let ran = run_in(&home, "folded:1", &["--rm"], &["report"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    assert!(ran.stdout.lines().any(|l| l == "uid 1000"), "{ran}");
    // A container made from it holds its config as Go read it.
    let made = shards(&["create", "--name", "folded-c", "folded:1"]);
    assert_eq!(made.status, Some(0), "{made}");
    let user = shards(&["inspect", "--format", "{{.Config.User}}", "folded-c"]);
    assert_eq!((user.status, user.stdout.as_str()), (Some(0), "app\n"), "{user}");
    // A config with no time Go reads lists at Go's zero time, as moby's summary has it,
    // which the CLI says was made at no time since (its CreatedAt is in the local zone).
    let listed = shards(&["images", "--format", "{{.CreatedSince}}|", "folded"]);
    assert_eq!(
        (listed.status, listed.stdout.as_str()),
        (Some(0), "|\n"),
        "{listed}"
    );

    load(
        "badhc:1",
        &config.replacen(r#""User":"app""#, r#""User":"app","Healthcheck":5"#, 1),
    );
    load("badhist:1", &config.replacen('{', r#"{"history":5,"#, 1));
    let hc = "could not deserialize image config: json: cannot unmarshal number into Go struct field DockerOCIImageConfig.config.DockerOCIImageConfigExt.Healthcheck of type v1.HealthcheckConfig";
    let ran = run_in(&home, "badhc:1", &["--rm"], &["report"]);
    assert_eq!(
        (ran.status, ran.stdout.as_str(), ran.stderr.as_str()),
        (
            Some(125),
            "",
            format!(
                "shards: Error response from daemon: {hc}\n\nRun 'shards run --help' for more information\n"
            )
            .as_str()
        ),
        "{ran}"
    );
    for args in [&["create", "badhc:1"][..], &["tag", "badhc:1", "other:1"]] {
        let said = shards(args);
        assert_eq!(
            (said.status, said.stderr.as_str()),
            (Some(1), format!("Error response from daemon: {hc}\n").as_str()),
            "{args:?}: {said}"
        );
    }
    for args in [&["image", "inspect", "badhc:1"][..], &["inspect", "badhc:1"]] {
        let said = shards(args);
        assert_eq!(
            (said.status, said.stdout.as_str(), said.stderr.as_str()),
            (
                Some(1),
                "[]\n",
                format!("Error response from daemon: failed to read image config: {hc}\n").as_str()
            ),
            "{args:?}: {said}"
        );
    }
    // `history` reads the rootfs and history alone: the healthcheck is not its to read.
    let history = shards(&["history", "-q", "badhc:1"]);
    assert_eq!(history.status, Some(0), "{history}");
    let history = shards(&["history", "badhist:1"]);
    assert_eq!(
        (history.status, history.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: could not deserialize image config: json: cannot unmarshal number into Go struct field .history of type []v1.History\n"
        ),
        "{history}"
    );
    let ran = run_in(&home, "badhist:1", &["--rm"], &["report"]);
    assert_eq!(
        (ran.status, ran.stderr.as_str()),
        (
            Some(125),
            "shards: Error response from daemon: could not deserialize image config: json: cannot unmarshal number into Go struct field DockerOCIImage.Image.history of type []v1.History\n\nRun 'shards run --help' for more information\n"
        ),
        "{ran}"
    );

    // A commit keeps what Go read of its image's config (daemon/containerd
    // image_commit.go): its history under a folded key; and its container's ports as
    // network.ParsePort wrote them when it was made.
    load(
        "based:1",
        &config
            .replacen('{', r#"{"History":[{"Created_By":"based step"}],"#, 1)
            .replacen(
                r#""User":"app""#,
                r#""User":"app","ExposedPorts":{"80/TCP":{}}"#,
                1,
            ),
    );
    let made = shards(&["create", "--name", "to-commit", "based:1"]);
    assert_eq!(made.status, Some(0), "{made}");
    let committed = shards(&["commit", "to-commit", "committed:1"]);
    assert_eq!(committed.status, Some(0), "{committed}");
    let history = shards(&["history", "--format", "{{.CreatedBy}}", "committed:1"]);
    assert!(history.stdout.lines().any(|l| l == "based step"), "{history}");
    let ports = shards(&[
        "image",
        "inspect",
        "--format",
        "{{json .Config.ExposedPorts}}",
        "committed:1",
    ]);
    assert_eq!(
        (ports.status, ports.stdout.as_str()),
        (Some(0), "{\"80/tcp\":{}}\n"),
        "{ports}"
    );
}
