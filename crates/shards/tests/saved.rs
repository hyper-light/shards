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
/// What a failing test says of microVM `name`: its state, as inspect gives it (its exit,
/// and whether the kernel killed its command for memory), and the end of its daemon's log,
/// where a microVM's end is said (CI's x86_64 runs 38808371825, 38812393591 and
/// 38907736104 lost this one's, a save finding it stopped, and a restore it went quiet in).
fn evidence(home: &Path, name: &str) -> String {
    let state = shards_in(home, &["inspect", "-f", "{{json .State}}", name]);
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines.get(lines.len().saturating_sub(60)..).unwrap_or_default();
    // What its guest does: its processes, their states and where they wait; and whether it
    // uses any CPU, sampled twice a second apart.
    let top = shards_in(home, &["top", name, "-o", "pid,stat,wchan:32,time,args"]);
    let stats = || {
        let s = shards_in(
            home,
            &[
                "stats",
                "--no-stream",
                "--format",
                "{{.CPUPerc}} {{.MemUsage}}",
                name,
            ],
        );
        format!("{}{}", s.stdout.trim(), s.stderr.trim())
    };
    // What its kernel does with memory, a second apart: swapping, reclaiming or stalled
    // (vmstat's pswpin, pswpout and pgscan, the pressure it is under, its cgroup's events),
    // and the host's, whose faults a guest's swap and restored memory take.
    let memory = || {
        let guest = shards_in(
            home,
            &[
                "exec",
                name,
                "/bin/testguest",
                "files",
                "/proc/vmstat",
                "/proc/meminfo",
                "/proc/pressure/memory",
                "@cgroup/memory.stat",
                "@cgroup/memory.events",
                "@cgroup/memory.pressure",
            ],
        );
        let host: String = ["/proc/pressure/memory", "/proc/pressure/io", "/proc/meminfo"]
            .iter()
            .map(|p| format!("== host {p}\n{}", std::fs::read_to_string(p).unwrap_or_default()))
            .collect();
        format!("{}{}{host}", guest.stdout, guest.stderr)
    };
    let first = stats();
    let first_memory = memory();
    std::thread::sleep(Duration::from_secs(1));
    let second = stats();
    let second_memory = memory();
    // Where each of its processes waits, in its kernel (proc(5)'s stack: privileged).
    let stacks = shards_in(
        home,
        &[
            "exec",
            "--privileged",
            "-u",
            "0",
            name,
            "/bin/testguest",
            "stacks",
        ],
    );
    format!(
        "its state: {}{}\nits processes:\n{}{}\nits use, a second apart: {first} | {second}\nits VM process's threads:\n{}\nits processes' stacks:\n{}{}\nits memory:\n{first_memory}\na second later:\n{second_memory}\nthe daemon's log's end:\n{}",
        state.stdout,
        state.stderr,
        top.stdout,
        top.stderr,
        vm_threads(&state.stdout),
        stacks.stdout,
        stacks.stderr,
        tail.join("\n")
    )
}

/// The VM process's threads as the host has them, on Linux: each one's name, state and the
/// kernel function it waits in (proc(5)), which say whether its vCPUs run or wait.
fn vm_threads(state: &str) -> String {
    let pid = serde_json::from_str::<serde_json::Value>(state)
        .ok()
        .and_then(|s| s["Pid"].as_u64())
        .filter(|&p| p > 0);
    let Some(pid) = pid else {
        return "(none running)".into();
    };
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return format!("(no /proc/{pid}/task on this host)");
    };
    let mut out = Vec::new();
    for task in tasks.flatten() {
        let dir = task.path();
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
        let stat = read("stat");
        // The state is the field after the name, which is in parentheses.
        let state = stat
            .rsplit_once(") ")
            .map(|(_, r)| r.chars().next().unwrap_or('?'));
        out.push(format!(
            "{} {} {} {}",
            task.file_name().to_string_lossy(),
            read("comm").trim(),
            state.unwrap_or('?'),
            read("wchan").trim()
        ));
    }
    out.sort();
    out.join("\n")
}

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
    // Files it holds open in its volume after removing them, whose bytes go with its
    // snapshot, past what a snapshot keeps (D176).
    let past = ((64u64 << 20) + 1).to_string();
    let (mut client, mut counts, _) = start(
        &home,
        &image,
        &["--name", "mounted", "-u", "0", "-v", "kept:/kept"],
        &["unlinked-held", "/kept/held", &past],
    );
    let refused = shards(&["save", "vm", "mounted", "first"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(refused.stderr.contains("more than a snapshot keeps"), "{refused}");
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
    // A file it holds open after removing it from the volume: its bytes go with the
    // snapshot, and the restored microVM reads them on through the same descriptor.
    let run = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "ghost",
        "-u",
        "0",
        "-v",
        "kept:/kept",
        &image,
        "unlinked-held",
        "/kept/held",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let read = || logs(&home, "ghost").matches("held").count();
    let said = || logs(&home, "ghost");
    until("reading", &mut || read() >= 3, &said);
    let saved = shards(&["save", "vm", "ghost", "held"]);
    assert_eq!(saved.stdout, "held\n", "{saved}");
    assert!(!home.join("volumes/kept/_data/held").exists());
    let at_save = read();
    let restored = shards(&["restore", "vm", "ghost", "held"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    until("reading on", &mut || read() >= at_save + 5, &said);
    let named: Vec<String> = std::fs::read_dir(home.join("tmp"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ghost-"))
        .collect();
    assert!(named.is_empty(), "no name left of it: {named:?}");
    assert_eq!(shards(&["rm", "-f", "ghost"]).status, Some(0));
}

/// A microVM whose volume's DAX window holds a file mapped (D189) is saved and restored
/// with its mappings (D176): its restore maps each range again from its new share process
/// before its guest runs, and the guest reads the file on through the window, a byte the
/// host wrote while it was saved among it.
#[test]
fn a_saved_microvm_reads_on_through_its_dax_window() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("saved-dax");
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_SHARE_WINDOW_MIB", "64".as_ref()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let dir = TempDir::new("saved-dax-v");
    let mut big: Vec<u8> = (0..8u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(dir.join("big"), &big).unwrap();
    let volume = format!("{}:/v", dir.display());
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "daxsave", "-u", "0", "-v", &volume, &image, "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let exec = |args: &[&str]| {
        let mut all = vec!["exec", "daxsave", "/bin/testguest"];
        all.extend_from_slice(args);
        let r = shards(&all);
        assert_eq!(r.status, Some(0), "{args:?}: {r}");
        r.stdout
    };
    assert_eq!(exec(&["fs", "dax:/v/big"]), "dax /v/big yes\n");
    let digest = |b: &[u8]| format!("digest /v/big {:016x}\n", shards_testguest::fnv1a(b));
    // Read whole: every range of it mapped.
    assert_eq!(exec(&["digest", "/v/big"]), digest(&big));
    let saved = shards(&["save", "vm", "daxsave", "mapped"]);
    assert_eq!(saved.stdout, "mapped\n", "{saved}");
    let at = 3 << 20;
    big[at] = big[at].wrapping_add(1);
    {
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("big"))
            .unwrap();
        file.write_all_at(&big[at..=at], at as u64).unwrap();
    }
    let restored = shards(&["restore", "vm", "daxsave", "mapped"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    assert_eq!(
        exec(&["fs", &format!("peek:/v/big:{at}")]),
        format!("peek /v/big {at} {}\n", big[at])
    );
    assert_eq!(exec(&["digest", "/v/big"]), digest(&big));
    assert_eq!(shards(&["rm", "-f", "daxsave"]).status, Some(0));
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
    until_in(what, TIMEOUT, done, said);
}

/// As [`until`], within `within`.
fn until_in(what: &str, within: Duration, done: &mut dyn FnMut() -> bool, said: &dyn Fn() -> String) {
    let deadline = Instant::now() + within;
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

/// A microVM saved as an exec in it writes on ends with its run once restored, by the
/// stop's own signal. The restore resets the exec's connection, its client's (D176), and
/// init read the exec's output only while less than BUFFERED of it waited to go: past
/// that, with no connection to take it, it read it no more, and the exec, killed with the
/// run, was never seen to end. The run outlived its stop's SIGTERM and SIGKILL alike until
/// its VM was killed (flakes plan step 3, PM M328).
#[test]
fn a_microvm_saved_as_an_exec_writes_on_ends_with_its_run_once_restored() {
    let Some((home, image)) = home("resumed-writer") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    // Under --init, so that its command ends at the stop's SIGTERM.
    let run = shards(&[
        "run", "-d", "--init", "--pull", "never", "--name", "writer", &image, "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let exec = shards(&["exec", "-d", "writer", "/bin/testguest", "count", "1000000000"]);
    assert_eq!(exec.status, Some(0), "{exec}");
    let saved = shards(&["save", "vm", "writer", "snap"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let restored = shards(&["restore", "vm", "writer"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    // A stop timeout past the test's, so that one its SIGKILL ended is told apart.
    let asked = Instant::now();
    let stopped = shards(&["stop", "-t", "30", "writer"]);
    let took = asked.elapsed();
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let code = shards(&["inspect", "-f", "{{.State.ExitCode}}", "writer"]);
    assert_eq!(
        (code.stdout.trim(), took < Duration::from_secs(30)),
        ("143", true),
        "stopped in {took:?}\n{}",
        evidence(&home, "writer")
    );
}

/// A microVM saved as an exec's stdin waits in its connection ends with its run once
/// restored, by the stop's own signal. The restore resets the exec's connection, its
/// client's (D176): one that held what init had not read stayed established in the guest,
/// read as at its end and never writable, the reset leaving it no credit. Init reads an
/// exec's connection no more while its stdin waits whole, and wrote the exec's end only as
/// the connection took it, so that the run outlived its stop's SIGTERM and SIGKILL until
/// its VM was killed; in CI a give-back the save cut short did so (flakes plan step 3, PM
/// M330).
#[test]
fn a_microvm_saved_as_an_execs_stdin_waits_ends_with_its_run_once_restored() {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    let Some((home, image)) = home("resumed-stdin") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run", "-d", "--init", "--pull", "never", "--name", "reader", &image, "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    // An exec that reads none of its stdin, given it until nothing takes more: its pipe,
    // what init holds for it, its connection and the client between them all full, the
    // rest of what its connection took unread in the guest as the save is taken.
    let (stdin, mut writer) = std::io::pipe().unwrap();
    // SAFETY: fcntl(2) setting the flags of a descriptor this test holds open.
    unsafe {
        let fd = writer.as_raw_fd();
        libc::fcntl(
            fd,
            libc::F_SETFL,
            libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
        );
    }
    let mut exec = common::command()
        .args(["exec", "-i", "reader", "/bin/testguest", "sleep"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let chunk = vec![b'x'; 64 << 10];
    let (mut given, mut full_since) = (0usize, None::<Instant>);
    while full_since.is_none_or(|at| at.elapsed() < Duration::from_secs(1)) {
        match writer.write(&chunk) {
            Ok(n) => {
                given += n;
                full_since = None;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                full_since.get_or_insert_with(Instant::now);
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("the exec's stdin: {e}"),
        }
        assert!(given < 64 << 20, "the exec took 64 MiB of stdin");
    }
    let saved = shards(&["save", "vm", "reader", "snap"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let _ = exit(&mut exec);
    let restored = shards(&["restore", "vm", "reader"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    // A stop timeout past the test's, so that one its SIGKILL ended is told apart.
    let asked = Instant::now();
    let stopped = shards(&["stop", "-t", "30", "reader"]);
    let took = asked.elapsed();
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let code = shards(&["inspect", "-f", "{{.State.ExitCode}}", "reader"]);
    assert_eq!(
        (code.stdout.trim(), took < Duration::from_secs(30)),
        ("143", true),
        "{given} bytes of stdin given; stopped in {took:?}\n{}",
        evidence(&home, "reader")
    );
}

/// An exec reading its stdin through a save ends once its microVM is restored: its stdin
/// ends with its client, which went with the microVM saved (D176), as a client's going
/// ends it while it runs. Before, it read on for ever from a stdin no one could write (PM
/// M330).
#[test]
fn an_exec_reading_its_stdin_through_a_save_ends_once_restored() {
    use std::io::{BufRead as _, Write as _};
    let Some((home, image)) = home("resumed-cat") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = shards(&[
        "run", "-d", "--init", "--pull", "never", "--name", "reader", &image, "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let (stdin, mut writer) = std::io::pipe().unwrap();
    let mut exec = common::command()
        .args(["exec", "-i", "reader", "/bin/testguest", "cat"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Reading as the save is taken: what it was given comes back.
    writer.write_all(b"before the save\n").unwrap();
    let mut echoed = String::new();
    BufReader::new(exec.stdout.take().unwrap())
        .read_line(&mut echoed)
        .unwrap();
    assert_eq!(echoed, "before the save\n");
    let saved = shards(&["save", "vm", "reader", "snap"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let _ = exit(&mut exec);
    let restored = shards(&["restore", "vm", "reader"]);
    assert_eq!(restored.status, Some(0), "{restored}");
    // The writer is still open here: the exec's stdin ends with its connection's client.
    let listed = || shards(&["exec", "reader", "/bin/testguest", "ps"]).stdout;
    let mut ps = listed();
    let deadline = Instant::now() + Duration::from_secs(10);
    while ps.contains("testguest cat") {
        assert!(
            Instant::now() < deadline,
            "an exec reading its stdin outlived its client by 10 s:\n{ps}"
        );
        std::thread::sleep(Duration::from_millis(100));
        ps = listed();
    }
    drop(writer);
    assert_eq!(shards(&["stop", "reader"]).status, Some(0));
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
    // Its rounds' times, which say whether they slowed and when.
    let said = || {
        let timed = shards(&["logs", "--timestamps", "swapped"]);
        format!("{}{}\n{}", timed.stdout, timed.stderr, evidence(&home, "swapped"))
    };
    // A round's time on this host, from its first to its second: what the rounds after the
    // restore are waited for by (CI's x86_64 runners took 20 s a round, CI 39249195872).
    until("its pattern kept", &mut || log().contains("kept 1\n"), &said);
    let first = Instant::now();
    until(
        "its pattern kept again",
        &mut || log().contains("kept 2\n"),
        &said,
    );
    let round = first.elapsed();
    let saved = shards(&["save", "vm", "swapped", "full"]);
    assert_eq!(saved.status, Some(0), "{saved}\n{}", evidence(&home, "swapped"));
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
    assert_eq!(
        restored.status,
        Some(0),
        "{restored}\n{}",
        evidence(&home, "swapped")
    );
    // The round the save cut short ends from what was restored, and the next reads all of
    // it back so.
    until_in(
        "its pattern kept after the restore",
        TIMEOUT + 2 * round,
        &mut || log().matches("kept ").count() >= rounds + 2,
        &said,
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
fn checkpoint_command_lines_save_list_and_restore_microvms() {
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
