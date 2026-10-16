//! MicroVMs saved whole as they run (D176), end to end: `shards save vm` of real
//! microVMs, from the test image a loopback registry serves (tests/common, `served`),
//! booted from SHARDS_KERNEL and SHARDS_INIT.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Lines};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::time::{Duration, Instant};

use common::{Run, TempDir, cannot_run_vms, cannot_snapshot, guest_init, kernel, run_shards_env, served};

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

/// An attached run of the test image left going, its command's output read line by
/// line, its stderr in the home: the client, the lines, and where it said what it said.
fn start(
    home: &Path,
    image: &str,
    options: &[&str],
    command: &[&str],
) -> (Child, Lines<BufReader<ChildStdout>>, PathBuf) {
    let said = home.join(format!("run-{}.stderr", options.join("-").replace('/', "_")));
    let mut child = common::command()
        .args(["run", "--pull", "never"])
        .args(options)
        .arg(image)
        .args(command)
        .env("SHARDS_HOME", home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&said).unwrap())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().map(Result::unwrap);
    assert_eq!(
        first.as_deref(),
        Some("ready"),
        "{command:?}: {}",
        std::fs::read_to_string(&said).unwrap_or_default()
    );
    (child, lines, said)
}

/// The count the counter says next.
fn next_count(lines: &mut Lines<BufReader<ChildStdout>>) -> u64 {
    lines.next().unwrap().unwrap().parse().unwrap()
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

/// The ID of the container `name`.
fn id_of(home: &Path, name: &str) -> String {
    let inspected = shards_in(home, &["inspect", "-f", "{{.Id}}", name]);
    assert_eq!(inspected.status, Some(0), "{inspected}");
    inspected.stdout.trim().to_string()
}

/// A running microVM is saved whole, as snapshot of its own: left running, it goes on
/// counting; stopped by its save, its client hears so, with the status a shell gives a
/// job it stopped, and `--rm` keeps it, snapshots and all. Names are a checkpoint's, each
/// once; a save without one takes the time's.
#[test]
fn a_running_microvm_is_saved_whole_left_running_or_stopped() {
    let Some((home, image)) = home("saved") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let (mut client, mut counts, said) =
        start(&home, &image, &["--name", "counter", "--rm"], &["counter", "20"]);
    if cannot_snapshot() {
        let refused = shards(&["save", "vm", "counter", "first"]);
        assert_eq!(refused.status, Some(1), "{refused}");
        assert!(refused.stderr.contains("take no snapshots yet"), "{refused}");
        let _ = shards(&["kill", "counter"]);
        let _ = exit(&mut client);
        return;
    }
    let before = next_count(&mut counts);
    let saved = shards(&["save", "vm", "counter", "first", "--leave-running"]);
    assert_eq!(
        (saved.status, saved.stdout.as_str()),
        (Some(0), "first\n"),
        "{saved}"
    );
    let snapshot = home
        .join("containers")
        .join(id_of(&home, "counter"))
        .join("snapshots")
        .join("first");
    for file in ["state", "memory", "swap"] {
        assert!(common::snapshot_file(&snapshot, file).is_file(), "{file}");
    }
    // It goes on counting where it was.
    let after = next_count(&mut counts);
    assert!(after > before, "{before} then {after}");
    assert_eq!(next_count(&mut counts), after + 1);
    // A name once; and a checkpoint's.
    let again = shards(&["save", "vm", "counter", "first", "--leave-running"]);
    assert_eq!(again.status, Some(1), "{again}");
    assert!(
        again.stderr.ends_with("has a snapshot named first already\n"),
        "{again}"
    );
    let bad = shards(&["save", "vm", "counter", "first."]);
    assert_eq!(bad.status, Some(1), "{bad}");
    assert!(bad.stderr.contains("cannot name a snapshot"), "{bad}");
    // Stopped by its save, as the time names it.
    let stopped = shards(&["save", "vm", "counter"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let name = stopped.stdout.trim().to_string();
    assert!(name.ends_with('Z') && !name.contains(':'), "{stopped}");
    assert!(common::snapshot_file(&snapshot.with_file_name(&name), "state").is_file());
    assert_eq!(exit(&mut client), Some(148));
    let told = std::fs::read_to_string(&said).unwrap();
    assert!(
        told.ends_with(&format!("shards: counter was saved as {name}, and stopped\n")),
        "{told}"
    );
    let inspected = shards(&[
        "inspect",
        "-f",
        "{{.State.Status}} {{.State.ExitCode}}",
        "counter",
    ]);
    assert_eq!(inspected.stdout, "exited 148\n", "{inspected}");
    let not = shards(&["save", "vm", "counter", "later"]);
    assert_eq!(not.status, Some(1), "{not}");
    assert!(not.stderr.ends_with("counter is not running\n"), "{not}");
    // Its snapshots, newest first; removed by name, each said as it goes.
    let listed = shards(&["list", "snapshots", "counter"]);
    assert_eq!(listed.status, Some(0), "{listed}");
    let rows: Vec<Vec<&str>> = listed
        .stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(rows[0], ["SNAPSHOT", "SAVED", "SIZE"], "{listed}");
    let names: Vec<&str> = rows[1..].iter().map(|r| r[0]).collect();
    assert_eq!(names, [name.as_str(), "first"], "{listed}");
    assert!(rows[1].contains(&"ago"), "{listed}");
    let removed = shards(&["remove", "snapshot", "counter", "first", "never"]);
    assert_eq!(
        (removed.status, removed.stdout.as_str()),
        (Some(1), "first\n"),
        "{removed}"
    );
    assert!(
        removed.stderr.ends_with("counter has no snapshot named never\n"),
        "{removed}"
    );
    let left = shards(&["list", "snapshots", "counter"]);
    assert_eq!(left.stdout.lines().count(), 2, "{left}");
}

/// A save a microVM cannot keep whole is refused, and the microVM goes on as it was, with
/// nothing of the save left: a volume's open files are its server's, which no snapshot
/// holds yet.
#[test]
fn a_save_refused_leaves_the_microvm_running_and_nothing_behind() {
    let Some((home, image)) = home("saved-refused") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    // A file it holds open in its volume, whose last name it removed, no path reaches, and
    // no restore could open (D176).
    let (mut client, mut counts, _) = start(
        &home,
        &image,
        &["--name", "mounted", "-u", "0", "-v", "kept:/kept"],
        &["unlinked-held", "/kept/held"],
    );
    let refused = shards(&["save", "vm", "mounted", "first"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused
            .stderr
            .contains("a file the microVM has open was removed from its volume"),
        "{refused}"
    );
    let snapshots = home
        .join("containers")
        .join(id_of(&home, "mounted"))
        .join("snapshots");
    let left: Vec<_> = std::fs::read_dir(&snapshots).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    // It goes on, and is stopped as ever.
    let _ = &mut counts;
    assert_eq!(shards(&["kill", "mounted"]).status, Some(0));
    let _ = exit(&mut client);
    // On the host's network its guest's sockets are the host's, which no snapshot keeps.
    let run = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "hosted",
        "--network",
        "host",
        &image,
        "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let refused = shards(&["save", "vm", "hosted"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(refused.stderr.contains("on the host's network"), "{refused}");
    assert_eq!(shards(&["rm", "-f", "hosted"]).status, Some(0));
    // Containers joined to it run in its microVM (D119), which a snapshot would keep
    // without their containers; once they end, it is saved.
    let run = |args: &[&str]| {
        let mut all = vec!["run", "-d", "--pull", "never"];
        all.extend_from_slice(args);
        let r = shards(&all);
        assert_eq!(r.status, Some(0), "{r}");
    };
    run(&["--name", "provider", &image, "sleep"]);
    run(&[
        "--name",
        "joiner",
        "--network",
        "container:provider",
        &image,
        "sleep",
    ]);
    let refused = shards(&["save", "vm", "--leave-running", "provider"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused
            .stderr
            .contains("containers joined to it run in its microVM"),
        "{refused}"
    );
    let snapshots = home
        .join("containers")
        .join(id_of(&home, "provider"))
        .join("snapshots");
    let left: Vec<_> = std::fs::read_dir(&snapshots).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(shards(&["rm", "-f", "joiner"]).status, Some(0));
    // Once they end, it is saved, and restored with its join disk as its guest knew it: a
    // container joins the restored microVM, its image given the disk's next range.
    let mut saved = shards(&["save", "vm", "provider", "joined"]);
    let deadline = Instant::now() + TIMEOUT;
    while saved.status != Some(0) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        saved = shards(&["save", "vm", "provider", "joined"]);
    }
    assert_eq!(saved.stdout, "joined\n", "{saved}");
    let restored = shards(&["restore", "vm", "provider", "joined"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    let joined = shards(&[
        "run",
        "--rm",
        "--pull",
        "never",
        "--network",
        "container:provider",
        &image,
        "fs",
        "write:/tmp/j=again",
        "print:/tmp/j",
    ]);
    assert_eq!(
        (joined.status, joined.stdout.as_str()),
        (Some(0), "again"),
        "{joined}"
    );
    assert_eq!(shards(&["rm", "-f", "provider"]).status, Some(0));
}

/// A microVM with a volume mounted is saved and restored with it (D176 part 5): the file
/// it holds open in the volume, in the volume's directory it works in, it writes on
/// through, its share's server given the tables of the one that served it; the volume holds
/// every count once, up to the save and on from it.
#[test]
fn a_saved_microvm_writes_on_through_its_volume() {
    let Some((home, image)) = home("saved-volume") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "vol",
        "-u",
        "0",
        "-v",
        "kept:/kept",
        &image,
        "appender",
        "/kept/counts",
        "20",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let file = home.join("volumes/kept/_data/counts");
    let written = || -> Vec<u64> {
        std::fs::read_to_string(&file)
            .unwrap_or_default()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect()
    };
    let log = || logs(&home, "vol");
    until("appending", &mut || written().len() >= 10, &log);
    let saved = shards(&["save", "vm", "vol", "first"]);
    assert_eq!(saved.stdout, "first\n", "{saved}");
    let at_save = written().len();
    let restored = shards(&["restore", "vm", "vol", "first"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    until("appending on", &mut || written().len() >= at_save + 10, &log);
    let counts = written();
    let wanted: Vec<u64> = (1..=counts.len() as u64).collect();
    assert_eq!(counts, wanted, "each count once, in order: {}", log());
    assert_eq!(shards(&["rm", "-f", "vol"]).status, Some(0));
}

/// `shards logs NAME`'s stdout.
fn logs(home: &Path, name: &str) -> String {
    let logs = shards_in(home, &["logs", name]);
    assert_eq!(logs.status, Some(0), "{logs}");
    logs.stdout
}

/// The counts a counter's log holds, in order.
fn counts(logs: &str) -> Vec<u64> {
    logs.lines().filter_map(|l| l.parse().ok()).collect()
}

/// Waits until `done`, failing the test after TIMEOUT with what `said` says then.
fn until(what: &str, done: &mut dyn FnMut() -> bool, said: &dyn Fn() -> String) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "{what}: {}", said());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A microVM its save stopped resumes where it was saved, from its newest snapshot or the
/// one named: its counter goes on from the count it had, its log holding each count once
/// up to the save and then each after it once more for each restore, nothing of its output
/// lost at the save or said twice there. A running one, or a snapshot it has not, is no
/// restore's.
#[test]
fn a_saved_microvm_resumes_where_it_was_saved() {
    let Some((home, image)) = home("resumed") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "20",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&home, "counter");
    until("counting", &mut || counts(&log()).len() >= 10, &log);
    let running = shards(&["restore", "vm", "counter"]);
    assert_eq!(running.status, Some(1), "{running}");
    assert!(running.stderr.contains("counter is running"), "{running}");
    let saved = shards(&["save", "vm", "counter", "first"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let before = counts(&log());
    let n = before.len() as u64;
    assert_eq!(
        before,
        (1..=n).collect::<Vec<_>>(),
        "each count once, up to the save"
    );
    let none = shards(&["restore", "vm", "counter", "second"]);
    assert_eq!(none.status, Some(1), "{none}");
    assert!(
        none.stderr.contains("counter has no snapshot named second"),
        "{none}"
    );
    // Its newest.
    let restored = shards(&["restore", "vm", "counter"]);
    assert_eq!(
        (restored.status, restored.stdout.as_str()),
        (Some(0), "counter\n"),
        "{restored}"
    );
    until("counting on", &mut || counts(&log()).len() as u64 >= n + 10, &log);
    let after = counts(&log());
    assert_eq!(
        after.get(..(n + 10) as usize),
        Some(&(1..=n + 10).collect::<Vec<_>>()[..]),
        "the count going on where it was saved"
    );
    let ps = shards(&["ps", "--filter", "name=counter", "--format", "{{.Status}}"]);
    assert!(ps.stdout.starts_with("Up "), "{ps}");
    // Stopped, and resumed again from the same snapshot, by name: from where it was saved.
    assert_eq!(shards(&["kill", "counter"]).status, Some(0));
    let ended = counts(&log()).len();
    let again = shards(&["restore", "vm", "counter", "first"]);
    assert_eq!(again.status, Some(0), "{again}");
    until(
        "counting from the save again",
        &mut || counts(&log()).len() >= ended + 5,
        &log,
    );
    let resumed = counts(&log());
    assert_eq!(
        resumed.get(ended..ended + 5),
        Some(&(n + 1..=n + 5).collect::<Vec<_>>()[..]),
        "{resumed:?}"
    );
    assert_eq!(shards(&["kill", "counter"]).status, Some(0));
}

/// A microVM saved and left running is restored from where it was saved, once stopped:
/// its counter goes on from the count of the save, not of its stop.
#[test]
fn a_microvm_saved_and_left_running_is_restored_from_its_save() {
    let Some((home, image)) = home("resumed-left") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "20",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&home, "counter");
    until("counting", &mut || counts(&log()).len() >= 5, &log);
    // The save's cut is between what its log held before it was asked for and what it
    // holds once it is done: left running, the microVM counts on as it is committed.
    let asked = counts(&log()).len() as u64;
    let saved = shards(&["save", "vm", "counter", "early", "--leave-running"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let at = counts(&log()).len() as u64;
    until(
        "counting on",
        &mut || counts(&log()).len() as u64 >= at + 10,
        &log,
    );
    assert_eq!(shards(&["kill", "counter"]).status, Some(0));
    let ended = counts(&log()).len();
    let restored = shards(&["restore", "vm", "counter", "early"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    until(
        "counting from its save",
        &mut || counts(&log()).len() >= ended + 3,
        &log,
    );
    let resumed = counts(&log());
    // From the count after the last its log held at the save's cut.
    let first = resumed[ended];
    assert!(
        (asked + 1..=at + 1).contains(&first),
        "{first} after a save between {asked} and {at}: {resumed:?}"
    );
    assert_eq!(resumed[ended + 1], first + 1, "{resumed:?}");
    assert_eq!(shards(&["kill", "counter"]).status, Some(0));
}

/// What a microVM swapped out under its memory limit comes back with it: saved, its swap
/// is kept beside its snapshot, and restored, a copy is mapped back before its guest runs.
#[test]
fn a_saved_microvm_keeps_what_it_swapped() {
    let Some((home, image)) = home("resumed-swap") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "swapped",
        "-m",
        "64m",
        &image,
        "swap-kept",
        "96",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&home, "swapped");
    until("its pattern kept", &mut || log().contains("kept 2\n"), &log);
    let saved = shards(&["save", "vm", "swapped", "full"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let snapshot = home
        .join("containers")
        .join(id_of(&home, "swapped"))
        .join("snapshots")
        .join("full");
    let swap = common::snapshot_file(&snapshot, "swap");
    let kept = std::fs::metadata(&swap).unwrap();
    assert!(kept.len() > 0, "no swap kept");
    let rounds = log().matches("kept ").count();
    let restored = shards(&["restore", "vm", "swapped"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    until(
        "its pattern kept after the restore",
        &mut || log().matches("kept ").count() >= rounds + 3,
        &log,
    );
    assert!(!log().contains("lost"), "{}", log());
    assert_eq!(shards(&["kill", "swapped"]).status, Some(0));
}

/// A microVM saved as it writes as fast as it can loses none of its output at the save's
/// cut, nor says any twice: all it held went to its log before it was saved, and what it
/// wrote after waits in it, for the copy restored, which says it from there.
#[test]
fn a_microvm_saved_as_it_writes_loses_none_of_its_output() {
    let Some((home, image)) = home("resumed-firehose") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "0",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&home, "counter");
    until("counting", &mut || counts(&log()).len() >= 2000, &|| {
        String::new()
    });
    let saved = shards(&["save", "vm", "counter", "midstream"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let before = counts(&log());
    let n = before.len() as u64;
    assert!(
        before == (1..=n).collect::<Vec<_>>(),
        "each count once, up to the save: {n} counts"
    );
    let restored = shards(&["restore", "vm", "counter"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    until(
        "counting on",
        &mut || counts(&log()).len() as u64 >= n + 2000,
        &|| String::new(),
    );
    let after = counts(&log());
    assert!(
        after.get(..(n + 2000) as usize) == Some(&(1..=n + 2000).collect::<Vec<_>>()[..]),
        "the count going on where it was saved, at {n}: {:?}",
        after.get(n.saturating_sub(3) as usize..(n + 3) as usize)
    );
    assert_eq!(shards(&["kill", "counter"]).status, Some(0));
}

/// A microVM on a network of its own is restored at its address there (D46, D176): its
/// guest holds it as it was saved, and its peers reach it at its name and that address,
/// its listening socket as it left it. Another member that took the address meanwhile
/// keeps it, and the restore says so.
#[test]
fn a_saved_microvm_on_a_network_is_restored_at_its_address() {
    let Some((home, image)) = home("resumed-net") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = shards(&["network", "create", "--subnet", "10.79.3.0/24", "savedlan"]);
    assert_eq!(made.status, Some(0), "{made}");
    // A member below it, gone by its restore: the lowest address free then is not its own.
    let below = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "below",
        "--network",
        "savedlan",
        &image,
        "sleep",
    ]);
    assert_eq!(below.status, Some(0), "{below}");
    let run = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "srv",
        "--network",
        "savedlan",
        &image,
        "serve",
        "7000",
        "5",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    until("serving", &mut || logs(&home, "srv").contains("ready\n"), &|| {
        logs(&home, "srv")
    });
    let address = |name: &str| {
        let r = shards(&[
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            name,
        ]);
        r.stdout.trim().to_string()
    };
    let ask = |at: &str| {
        let mut args: Vec<String> = ["run", "--rm", "--pull", "never", "--network", "savedlan"]
            .map(String::from)
            .to_vec();
        args.extend([image.clone(), "ask".into(), format!("{at}:7000")]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        shards(&args)
    };
    let saved_at = address("srv");
    assert!(saved_at.starts_with("10.79.3."), "{saved_at}");
    let asked = ask("srv");
    assert!(asked.stdout.starts_with("ask from "), "{asked}");
    let saved = shards(&["save", "vm", "srv", "serving"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    assert_eq!(shards(&["rm", "-f", "below"]).status, Some(0));
    let restored = shards(&["restore", "vm", "srv"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    assert_eq!(address("srv"), saved_at);
    for at in ["srv", saved_at.as_str()] {
        let asked = ask(at);
        assert!(asked.stdout.starts_with("ask from "), "{at}: {asked}");
    }
    // Stopped, its address is another's to take: then its restore fails, and says why.
    assert_eq!(shards(&["kill", "srv"]).status, Some(0));
    let taker = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "taker",
        "--network",
        "savedlan",
        "--ip",
        &saved_at,
        &image,
        "sleep",
    ]);
    assert_eq!(taker.status, Some(0), "{taker}");
    let refused = shards(&["restore", "vm", "srv"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(refused.stderr.contains(&saved_at), "{refused}");
    assert_eq!(shards(&["rm", "-f", "taker"]).status, Some(0));
}

/// Docker's words for it, as an experimental dockerd serves them: `checkpoint create`,
/// `ls` and `rm`, and `start --checkpoint`, a microVM's snapshots by another name, each
/// answered as docker/cli and dockerd answer.
#[test]
fn checkpoints_are_a_microvms_snapshots_in_dockers_words() {
    let Some((home, image)) = home("checkpoints") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "cp", &image, "counter", "20",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&home, "cp");
    until("counting", &mut || counts(&log()).len() >= 10, &log);
    let made = shards(&["checkpoint", "create", "cp", "first"]);
    assert_eq!(
        (made.status, made.stdout.as_str()),
        (Some(0), "first\n"),
        "{made}"
    );
    let n = counts(&log()).len() as u64;
    let listed = shards(&["checkpoint", "ls", "cp"]);
    assert_eq!(listed.stdout, "CHECKPOINT NAME\nfirst\n", "{listed}");
    // Restored by name, saying nothing, as docker/cli says nothing of it.
    let started = shards(&["start", "--checkpoint", "first", "cp"]);
    assert_eq!(
        (started.status, started.stdout.as_str()),
        (Some(0), ""),
        "{started}"
    );
    until("counting on", &mut || counts(&log()).len() as u64 >= n + 5, &log);
    assert_eq!(counts(&log())[n as usize], n + 1);
    let left = shards(&["checkpoint", "create", "--leave-running", "cp", "second"]);
    assert_eq!(left.stdout, "second\n", "{left}");
    let again = shards(&["checkpoint", "create", "--leave-running", "cp", "second"]);
    assert_eq!(again.status, Some(1), "{again}");
    assert_eq!(
        again.stderr,
        "shards: cannot checkpoint container cp: checkpoint with name second already exists for container cp\n"
    );
    // A name that would leave its microVM's snapshots is no name, refused before anything
    // is looked up, as dockerd refuses it.
    let bad = shards(&["checkpoint", "create", "cp", "../x"]);
    assert_eq!(bad.status, Some(1), "{bad}");
    assert_eq!(
        bad.stderr,
        "shards: invalid checkpoint ID \"../x\", only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed\n"
    );
    let nowhere = shards(&["checkpoint", "rm", "nothing", "../x"]);
    assert!(
        nowhere.stderr.contains("invalid checkpoint ID \"../x\""),
        "{nowhere}"
    );
    let removed = shards(&["checkpoint", "rm", "cp", "first"]);
    assert_eq!(
        (removed.status, removed.stdout.as_str()),
        (Some(0), ""),
        "{removed}"
    );
    let gone = shards(&["checkpoint", "rm", "cp", "first"]);
    assert_eq!(
        gone.stderr, "shards: checkpoint first does not exist for container cp\n",
        "{gone}"
    );
    assert_eq!(
        shards(&["checkpoint", "ls", "cp"]).stdout,
        "CHECKPOINT NAME\nsecond\n"
    );
    // A directory of its own is no microVM's: shards keeps its snapshots with it, and
    // dockerd restores from no other.
    let elsewhere = shards(&[
        "checkpoint",
        "create",
        "--checkpoint-dir",
        "/tmp/cps",
        "cp",
        "third",
    ]);
    assert_ne!(elsewhere.status, Some(0), "{elsewhere}");
    assert!(
        elsewhere
            .stderr
            .contains("\"--checkpoint-dir\": shards keeps a microVM's checkpoints with it"),
        "{elsewhere}"
    );
    let many = shards(&["start", "--checkpoint", "second", "cp", "other"]);
    assert_eq!(
        many.stderr, "you cannot restore multiple containers at once\n",
        "{many}"
    );
    // What a daemon's end cut short of a save, a snapshot with no generation in place, the
    // next daemon removes as it starts, its name free again; the saved ones stay.
    let id = shards(&["inspect", "-f", "{{.Id}}", "cp"])
        .stdout
        .trim()
        .to_string();
    let cut = home.join("containers").join(&id).join("snapshots").join("cut");
    std::fs::create_dir_all(cut.join(".g-1-1-1.tmp")).unwrap();
    std::fs::write(cut.join(".g-1-1-1.tmp").join("memory"), b"half").unwrap();
    assert_eq!(shards(&["stop", "daemon"]).status, Some(0));
    assert_eq!(
        shards(&["checkpoint", "ls", "cp"]).stdout,
        "CHECKPOINT NAME\nsecond\n"
    );
    assert!(!cut.exists(), "{} was left", cut.display());
    assert_eq!(shards(&["rm", "-f", "cp"]).status, Some(0));
}
