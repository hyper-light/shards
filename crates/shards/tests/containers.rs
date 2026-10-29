//! Containers, end to end: what `shards run` and `run -d` leave behind, and `shards ps`,
//! `wait`, `logs`, `rm`, `stop` and `kill` (crates/shards/src/daemon/commands.rs), as
//! `docker` has them. Real VMs, booted from SHARDS_KERNEL and SHARDS_INIT, so no snapshots
//! are needed; the image comes from a loopback registry (tests/common, `served`), and its
//! entrypoint is the test guest. Tests of what Windows serves with a later step of the
//! platform parity plan say so (`later_on_windows`), or are Unix's until then where they
//! reach Unix's own interfaces, each naming its step.

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
    served_counting, sha256_digest, test_index,
};
#[cfg(unix)]
use common::{shards, shards_vm};

const TIMEOUT: Duration = Duration::from_secs(60);

/// `shards ARGS` in `home`, whose runs boot the test kernel and init.
fn shards_in(home: &Path, args: &[&str]) -> Run {
    shards_in_within(home, args, TIMEOUT)
}

/// [`shards_in`] with a timeout of its own, for a run whose work is heavier than a start:
/// a large `-m` whose guest plugs and touches gigabytes takes far longer than [`TIMEOUT`]
/// on a slow, contended CI runner (it progresses, it is not hung: the whole of
/// `run_limits_a_microvms_resources` took 199 s under 32 busy loops where it took 30 idle).
fn shards_in_within(home: &Path, args: &[&str], timeout: Duration) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    run_shards_env(&[], args, &env, timeout)
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
    run_in_within(home, image, options, command, TIMEOUT)
}

/// [`run_in`] with a timeout of its own ([`shards_in_within`]), for a memory-heavy run.
fn run_in_within(home: &Path, image: &str, options: &[&str], command: &[&str], timeout: Duration) -> Run {
    let args = run_args(image, options, command);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    shards_in_within(home, &args, timeout)
}

/// What `home`'s daemon logged of processes ended for want of memory, for a failure's
/// message: init's facts of each (its cgroup's memory files, and the guest kernel's own
/// words, which say whether the cgroup's limit ran out or the microVM's memory did; D159).
/// A test's home goes as it drops, a failing test's too, and its log with it.
fn oom_said(home: &Path) -> String {
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    log.lines()
        .filter(|l| l.contains("for want of memory"))
        .map(|l| format!("\n  daemon.log: {l}"))
        .collect()
}

/// A run left going: its client, with the command's first line read.
fn start(home: &Path, image: &str, options: &[&str], command: &[&str]) -> Child {
    // In the home, and shown where the run ends unready ([`ended`]).
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let said = home.join(format!("run-{n}.stderr"));
    let stderr = std::fs::File::create(&said).unwrap();
    let mut child = common::interruptible(
        common::command()
            .args(run_args(image, options, command))
            .env("SHARDS_HOME", home)
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr),
    )
    .spawn()
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    if line != "ready\n" {
        panic!(
            "{command:?} ended unready ({line:?}): {}",
            ended(&mut child, Some(&said))
        );
    }
    child
}

/// How a run's client that should have gone on ended, and what it said where it said it
/// into `stderr`: a test's home goes with it, failing or not (common::TempDir).
fn ended(child: &mut Child, stderr: Option<&Path>) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                #[cfg(unix)]
                let signal = std::os::unix::process::ExitStatusExt::signal(&s);
                #[cfg(windows)]
                let signal: Option<i32> = None;
                break format!("status {:?}, signal {signal:?}", s.code());
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                break "still running 10 s after its output ended".to_string();
            }
            Err(e) => break format!("its status unknown: {e}"),
        }
    };
    let said = stderr
        .map(|p| std::fs::read_to_string(p).unwrap_or_else(|e| format!("{}: {e}", p.display())))
        .unwrap_or_default();
    format!("{status}\n--- stderr\n{said}")
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

/// `--network container:NAME` (D119): the joiner runs in the provider's microVM, in its
/// network namespace, as Docker's joiner runs in the provider's: it reaches the provider's
/// listener on the loopback, and has its hostname.
#[test]
fn a_container_joins_another_containers_network() {
    let Some((home, image)) = home("containers-join") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["serve", "7000", "1"]);
    let joined = run_in(
        &home,
        &image,
        &["--rm", "--network", "container:prov"],
        &["ask", "127.0.0.1:7000"],
    );
    assert_eq!(
        (joined.status, joined.stdout.as_str()),
        (Some(0), "ask from 127.0.0.1\n"),
        "{joined}"
    );
    assert_eq!(exit(&mut provider), Some(0));
}

/// A joiner is a container of its own (D119): detached, its log kept; an exec in it runs
/// in its namespaces, with its provider's hostname; `inspect` says its network mode as
/// given and its provider's hostname; it stops alone, its provider running on. What
/// dockerd refuses as a joiner is made or started is refused in its words, and what
/// shards does not do for a joiner yet, by name.
#[test]
fn a_joiner_is_a_container_of_its_own() {
    let Some((home, image)) = home("containers-joiner") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["serve", "7000", "1"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let detached = run_in(
        &home,
        &image,
        &["-d", "--name", "joiner", "--network", "container:prov"],
        &["trap", "TERM"],
    );
    assert_eq!(detached.status, Some(0), "{detached}");
    let deadline = Instant::now() + TIMEOUT;
    while !shards(&["logs", "joiner"]).stdout.contains("ready") {
        assert!(Instant::now() < deadline, "the joiner never said it was ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    let hostname = shards(&["inspect", "-f", "{{.Config.Hostname}}", "prov"]);
    let hostname = hostname.stdout.trim().to_string();
    assert_eq!(hostname.len(), 12, "the provider's hostname: {hostname:?}");
    // Its provider kept by its ID, as dockerd keeps it (D119; adaptSharedNamespaceContainer).
    let id = shards(&["inspect", "-f", "{{.Id}}", "prov"]).stdout;
    let inspected = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.NetworkMode}} {{.Config.Hostname}}",
        "joiner",
    ]);
    assert_eq!(
        inspected.stdout,
        format!("container:{} {hostname}\n", id.trim()),
        "{inspected}"
    );
    let reported = shards(&["exec", "joiner", "/bin/testguest", "report"]);
    assert_eq!(reported.status, Some(0), "{reported}");
    assert!(
        reported
            .stdout
            .lines()
            .any(|l| l == format!("hostname {hostname}")),
        "{reported}"
    );
    let wrote = shards(&[
        "exec",
        "-u",
        "0",
        "prov",
        "/bin/testguest",
        "fs",
        "write:/served=1",
    ]);
    assert_eq!(wrote.status, Some(0), "{wrote}");
    let asked = shards(&["exec", "joiner", "/bin/testguest", "ask", "127.0.0.1:7000"]);
    assert_eq!(asked.stdout, "ask from 127.0.0.1\n", "{asked}");
    // The provider served its one connection and ended; its joiner goes on.
    assert_eq!(exit(&mut provider), Some(0));
    let running = shards(&["inspect", "-f", "{{.State.Status}}", "joiner"]);
    assert_eq!(running.stdout, "running\n", "{running}");
    // Its stop signal reaches its own command, on its own connection.
    let stopped = shards(&["stop", "joiner"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let exited = shards(&["inspect", "-f", "{{.State.Status}} {{.State.ExitCode}}", "joiner"]);
    assert_eq!(exited.stdout, "exited 0\n", "{exited}");
    let logged = shards(&["logs", "joiner"]);
    assert_eq!(logged.stdout, "ready\ngot 15\n", "{logged}");
    let read = shards(&["read", "logs", "joiner"]);
    assert_eq!(read.stdout, logged.stdout, "shards' own words: {read}");
    // The provider's files, saved as it ended beside its joiner, read with their microVM
    // gone (D37).
    let kept = shards(&["diff", "prov"]);
    assert_eq!(
        (kept.status, kept.stdout.as_str()),
        (Some(0), "A /served\n"),
        "{kept}"
    );
    let read = shards(&["read", "changes", "prov"]);
    assert_eq!(read.stdout, kept.stdout, "shards' own words: {read}");
    // dockerd's refusals, as the container is made and as it starts.
    let refused = |options: &[&str], said: &str| {
        let r = run_in(&home, &image, options, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{options:?}: {r}");
        assert!(r.stderr.contains(said), "{options:?}: {r}");
    };
    refused(
        &["--network", "container:joiner", "-p", "80"],
        "-p: a run joining another's microVM takes that run's ports: publish them on that run",
    );
    refused(
        &["--network", "container:joiner", "--hostname", "h"],
        "--hostname: a run joining another's microVM takes that run's hostname",
    );
    refused(
        &["--network", "container:joiner"],
        "--network container:joiner: joiner is exited, and a run joins a running one's microVM",
    );
    refused(
        &["--network", "container:nope"],
        "--network container:nope: No such container: nope",
    );
}

/// The mode a guest reads of a file the host wrote, as `stat` prints it: its own on Unix,
/// and on Windows, which has no mode, 0755 (D197).
fn host_file_mode() -> &'static str {
    if cfg!(windows) { "755" } else { "644" }
}

/// A joiner's volumes, as a container's (D119), through its microVM's join share: a host
/// directory shared both ways, read-only where asked, a file bound alone, a named volume
/// filled from its image where it is empty, its image's VOLUME; each its own, neither its
/// provider's nor another joiner's; given again as it starts again, as it left them.
#[test]
fn a_joiner_takes_volumes_as_a_container_does() {
    let Some((home, image)) = home("containers-joiner-volumes") else {
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
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let joining = |options: &[&'static str]| -> Vec<&'static str> {
        ["--network", "container:prov"]
            .into_iter()
            .chain(options.iter().copied())
            .collect()
    };
    let with = |options: &[&str], more: &[&str]| -> Vec<String> {
        joining(&[])
            .into_iter()
            .chain(options.iter().copied())
            .chain(more.iter().copied())
            .map(String::from)
            .collect()
    };
    let run = |options: Vec<String>, command: &[&str]| {
        let options: Vec<&str> = options.iter().map(String::as_str).collect();
        run_in(&home, &image, &options, command)
    };
    let ran = run(
        with(
            &["--rm", "-v", &bind, "-v", &ro, "-v", &single],
            &["-v", "tools:/bin"],
        ),
        &["stat", "/data/a", "/etc/one", "/proc/self/mounts"],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let host = host_file_mode();
    assert!(
        ran.stdout
            .contains(&format!("/data/a file {host} 0:0 4\n= host\n")),
        "{ran}"
    );
    assert!(
        ran.stdout
            .contains(&format!("/etc/one file {host} 0:0 5\n= alone\n")),
        "{ran}"
    );
    for mount in [
        "shards-join-share /data virtiofs rw,",
        "shards-join-share /ro virtiofs ro,",
        "shards-join-share /etc/one virtiofs ro,",
        "shards-join-share /bin virtiofs rw,",
    ] {
        assert!(ran.stdout.contains(mount), "{mount}: {ran}");
    }
    // The named volume keeps what the image had at /bin, copied as it was first mounted.
    assert!(
        home.join("volumes/tools/_data/testguest").exists(),
        "the image's /bin in the volume"
    );
    // Written by the joiner, there on the host; read-only where asked; the file alone,
    // nothing else of its directory.
    let wrote = run(
        with(&["--rm", "-u", "0", "-v", &bind], &[]),
        &["fs", "mkdir:/data/d", "write:/data/d/b=joiner"],
    );
    assert_eq!(wrote.status, Some(0), "{wrote}");
    assert_eq!(std::fs::read_to_string(shared.join("d/b")).unwrap(), "joiner");
    let refused = run(
        with(&["--rm", "-u", "0", "-v", &ro], &[]),
        &["fs", "write:/ro/x=1"],
    );
    assert!(
        refused.stderr.contains("write:/ro/x=1: Read-only file system"),
        "{refused}"
    );
    let alone = run(with(&["--rm", "-v", &single], &[]), &["stat", "/etc/one/../a"]);
    assert_eq!(alone.status, Some(1), "{alone}");
    // Each its own: another joiner of the microVM, and its provider, see none of them.
    let held = run(with(&["-d", "--name", "j1", "-v", &bind], &[]), &["sleep"]);
    assert_eq!(held.status, Some(0), "{held}");
    let other = run(with(&["--rm"], &[]), &["stat", "/data/a"]);
    assert_eq!(other.status, Some(1), "{other}");
    let theirs = shards(&["exec", "prov", "/bin/testguest", "stat", "/data/a"]);
    assert_eq!(theirs.status, Some(1), "{theirs}");
    let own = shards(&["exec", "j1", "/bin/testguest", "stat", "/data/a"]);
    assert_eq!(own.status, Some(0), "{own}");
    // `cp` reads a volume where the joiner has it, as Docker's reads a container's (D119).
    let copied = shards(&["cp", "j1:/data/a", "-"]);
    assert_eq!(copied.status, Some(0), "{copied}");
    assert!(copied.stdout.contains("host"), "{copied}");
    // Started again, a joiner has its volumes again, as it left them.
    let made = run(with(&["-d", "--name", "again", "-v", "kept:/v"], &[]), &["sleep"]);
    assert_eq!(made.status, Some(0), "{made}");
    let wrote = shards(&["exec", "-u", "0", "again", "/bin/testguest", "fs", "write:/v/x=1"]);
    assert_eq!(wrote.status, Some(0), "{wrote}");
    assert_eq!(shards(&["stop", "-t", "1", "again"]).status, Some(0));
    assert_eq!(shards(&["start", "again"]).status, Some(0));
    let kept = shards(&["exec", "again", "/bin/testguest", "fs", "print:/v/x"]);
    assert_eq!((kept.status, kept.stdout.as_str()), (Some(0), "1"), "{kept}");
    // Its image's VOLUME: an anonymous volume, filled from the image.
    let ctx = home.join("volumed");
    std::fs::create_dir(&ctx).unwrap();
    std::fs::write(
        ctx.join("Dockerfile"),
        format!("FROM {image}\nCOPY seed /cache/seed\nVOLUME /cache\n"),
    )
    .unwrap();
    std::fs::write(ctx.join("seed"), "seeded").unwrap();
    let built = shards(&["build", "-q", "-t", "volumed:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{built}");
    let options: Vec<&str> = joining(&["--rm"]);
    let ran = run_in(
        &home,
        "volumed:1",
        &options,
        &["stat", "/cache/seed", "/proc/self/mounts"],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    // A Windows context's file is 0755, as fsutil sends one (shards-build's host.rs).
    let seeded = if cfg!(windows) { "755" } else { "644" };
    assert!(
        ran.stdout
            .contains(&format!("/cache/seed file {seeded} 0:0 6\n= seeded\n")),
        "{ran}"
    );
    assert!(
        ran.stdout.contains("shards-join-share /cache virtiofs rw,"),
        "{ran}"
    );
    for name in ["j1", "again"] {
        assert_eq!(shards(&["stop", "-t", "1", name]).status, Some(0));
    }
    assert_eq!(shards(&["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
}

/// A joiner's own commands reach the joiner (D119), never its provider: a client attached
/// to it writes its stdin and reads its output, and hears its status; `top` lists its
/// processes alone, `cp` copies from its root (its provider's hostname file bound there),
/// `stats` samples its cgroup, `diff` and `commit` read its own layers.
#[test]
fn a_joiners_own_commands_reach_the_joiner() {
    use std::io::Write as _;
    let Some((home, image)) = home("containers-joiner-own") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let joiner = run_in(
        &home,
        &image,
        &["-d", "-i", "--name", "joiner", "--network", "container:prov"],
        &["cat"],
    );
    assert_eq!(joiner.status, Some(0), "{joiner}");
    let mut attached = common::command()
        .args(["attach", "joiner"])
        .env("SHARDS_HOME", home.as_os_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // What it writes comes back through the joiner's `cat`: its stdin and output both the
    // joiner's.
    let (lines, read) = std::sync::mpsc::channel();
    let out = attached.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            if lines.send(line.unwrap()).is_err() {
                return;
            }
        }
    });
    attached.stdin.as_mut().unwrap().write_all(b"hello\n").unwrap();
    assert_eq!(read.recv_timeout(TIMEOUT).unwrap(), "hello");
    // Paused alone, its cgroup frozen in the guest as Docker freezes a container's (D119): what is
    // written meanwhile comes back once it goes on, and its provider runs on.
    let paused = shards(&["pause", "joiner"]);
    assert_eq!(paused.status, Some(0), "{paused}");
    let state = |name: &str| shards(&["inspect", "-f", "{{.State.Status}}", name]).stdout;
    assert_eq!(
        (state("joiner"), state("prov")),
        ("paused\n".into(), "running\n".into())
    );
    attached.stdin.as_mut().unwrap().write_all(b"after\n").unwrap();
    assert!(
        read.recv_timeout(Duration::from_millis(500)).is_err(),
        "a frozen joiner echoed"
    );
    let unpaused = shards(&["unpause", "joiner"]);
    assert_eq!(unpaused.status, Some(0), "{unpaused}");
    assert_eq!(read.recv_timeout(TIMEOUT).unwrap(), "after");
    let top = shards(&["top", "joiner", "-o", "pid,args"]);
    assert_eq!(top.status, Some(0), "{top}");
    assert!(
        top.stdout.contains("testguest cat") && !top.stdout.contains("testguest sleep"),
        "{top}"
    );
    let hostname = shards(&["inspect", "-f", "{{.Config.Hostname}}", "prov"]);
    let copied = shards(&["cp", "joiner:/etc/hostname", "-"]);
    assert_eq!(copied.status, Some(0), "{copied}");
    assert!(copied.stdout.contains(hostname.stdout.trim()), "{copied}");
    let stats = shards(&[
        "stats",
        "--no-stream",
        "--format",
        "{{.Name}} {{.PIDs}}",
        "joiner",
    ]);
    assert_eq!(stats.stdout, "joiner 1\n", "{stats}");
    // Its own changes, from the layers its root was built of: diff lists them, as
    // Docker's, none from what made its root; commit keeps them in an image, freezing its
    // cgroup alone meanwhile.
    let fresh = shards(&["diff", "joiner"]);
    assert_eq!((fresh.status, fresh.stdout.as_str()), (Some(0), ""), "{fresh}");
    let wrote = shards(&[
        "exec",
        "-u",
        "0",
        "joiner",
        "/bin/testguest",
        "fs",
        "write:/made=by-joiner",
        "write:/etc/variant=x",
        "rm:/etc/group",
    ]);
    assert_eq!(wrote.status, Some(0), "{wrote}");
    let diff = shards(&["diff", "joiner"]);
    assert_eq!(
        (diff.status, diff.stdout.as_str()),
        (Some(0), "C /etc\nD /etc/group\nA /etc/variant\nA /made\n"),
        "{diff}"
    );
    let committed = shards(&["commit", "joiner", "joined:1"]);
    assert_eq!(committed.status, Some(0), "{committed}");
    let kept = run_in(&home, "joined:1", &["--rm"], &["fs", "print:/made"]);
    assert_eq!(kept.stdout, "by-joiner", "{kept}");
    // Its command, PID 1 of its namespace, has no handler for SIGTERM: killed past the
    // timeout, as Docker's would be (D119); the client attached hears its status.
    let stopped = shards(&["stop", "-t", "1", "joiner"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    assert_eq!(exit(&mut attached), Some(137));
    assert_eq!(shards(&["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
}

/// A joiner may share its provider's PID namespace, `--pid container:NAME` with `--network
/// container:NAME`, as Docker's may (D119; Docker 29.3.1, PM M169): its PID 1 is its
/// provider's command; `top` lists each container's own processes, by its cgroup; `--init`
/// is taken and not given; and what it started is killed as it ends. Both are kept by the
/// provider's ID. Another's PID namespace alone is refused, a microVM's being its own
/// (D115), and one not there is refused in dockerd's words.
#[test]
fn a_joiner_may_share_its_providers_pid_namespace() {
    let Some((home, image)) = home("containers-joiner-pid") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let id = shards(&["inspect", "-f", "{{.Id}}", "prov"]).stdout;
    let id = id.trim();
    let joined = run_in(
        &home,
        &image,
        &[
            "-d",
            "--name",
            "joiner",
            "--network",
            "container:prov",
            "--pid",
            "container:prov",
            "--init",
        ],
        &["trap", "TERM"],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    let modes = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.NetworkMode}} {{.HostConfig.PidMode}} {{.HostConfig.Init}}",
        "joiner",
    ]);
    assert_eq!(
        modes.stdout,
        format!("container:{id} container:{id} true\n"),
        "{modes}"
    );
    let first = shards(&["exec", "joiner", "/bin/testguest", "fs", "print:/proc/1/cmdline"]);
    assert_eq!(first.stdout, "/bin/testguest\0sleep\0", "{first}");
    let top = |name: &str| shards(&["top", name, "-o", "pid,args"]).stdout;
    let (theirs, ours) = (top("prov"), top("joiner"));
    assert!(
        theirs.contains("testguest sleep") && !theirs.contains("testguest trap"),
        "{theirs}"
    );
    assert!(
        ours.contains("testguest trap") && !ours.contains("testguest sleep") && !ours.contains("init"),
        "{ours}"
    );
    // What it started, an exec of its, goes as it ends; seen from its provider.
    let left = shards(&["exec", "-d", "joiner", "/bin/testguest", "fs", "sleep:600000"]);
    assert_eq!(left.status, Some(0), "{left}");
    let seen = || shards(&["exec", "prov", "/bin/testguest", "ps"]).stdout;
    let deadline = Instant::now() + TIMEOUT;
    while !seen().contains("fs sleep:600000") {
        assert!(Instant::now() < deadline, "the exec never showed: {}", seen());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(shards(&["stop", "-t", "1", "joiner"]).status, Some(0));
    let deadline = Instant::now() + TIMEOUT;
    while seen().contains("fs sleep:600000") {
        assert!(
            Instant::now() < deadline,
            "what the joiner started outlived it: {}",
            seen()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The microVM's own, with `--pid host`: init its PID 1.
    let host = run_in(
        &home,
        &image,
        &["--rm", "--network", "container:prov", "--pid", "host"],
        &["fs", "print:/proc/1/cmdline"],
    );
    assert_eq!(host.status, Some(0), "{host}");
    assert!(!host.stdout.starts_with("/bin/testguest"), "{host}");
    // Refused: another's PID namespace alone, and one that is not there.
    let refused = |options: &[&str], said: &str| {
        let r = run_in(&home, &image, options, &["exit", "0"]);
        assert_eq!(
            (r.status, r.stderr.contains(said)),
            (Some(125), true),
            "{options:?}: {r}"
        );
    };
    refused(
        &["--pid", "container:prov"],
        "--pid container:prov: a run shares another's PID namespace inside that run's microVM, which it joins with --network container:prov",
    );
    refused(
        &["--pid", "container:nope"],
        "--pid container:nope: no run is named nope",
    );
    assert_eq!(shards(&["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
}

/// A joiner may share another joiner's PID namespace, as Docker's `--pid container:NAME`
/// takes that of NAME's process (D119): its PID 1 is that joiner's command; a container
/// naming it is in the same namespace; one naming a joiner in its provider's is in its
/// provider's; and as the joiner whose namespace it is ends, so does all in it, as the
/// kernel ends a namespace whose first process has gone, and as Docker's ends (D119).
#[test]
fn a_joiner_may_share_another_joiners_pid_namespace() {
    let Some((home, image)) = home("containers-joiner-pid-joiner") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = |options: &[&str], command: &[&str]| {
        let r = run_in(&home, &image, options, command);
        assert_eq!(r.status, Some(0), "{options:?}: {r}");
        r
    };
    run(
        &["-d", "--name", "j1", "--network", "container:prov"],
        &["trap", "TERM"],
    );
    run(
        &[
            "-d",
            "--name",
            "j2",
            "--network",
            "container:j1",
            "--pid",
            "container:j1",
        ],
        &["fs", "sleep:600000"],
    );
    let first = shards(&["exec", "j2", "/bin/testguest", "fs", "print:/proc/1/cmdline"]);
    assert_eq!(first.stdout, "/bin/testguest\0trap\0TERM\0", "{first}");
    // Through j2, in j1's: the same namespace, and not the provider's.
    let listed = run(
        &["--rm", "--network", "container:j2", "--pid", "container:j2"],
        &["ps"],
    );
    assert!(
        listed.stdout.contains("testguest trap TERM")
            && listed.stdout.contains("testguest fs sleep:600000")
            && !listed.stdout.contains("testguest sleep"),
        "{listed}"
    );
    // A joiner in its provider's namespace passes that one on.
    run(
        &[
            "-d",
            "--name",
            "j3",
            "--network",
            "container:prov",
            "--pid",
            "container:prov",
        ],
        &["fs", "sleep:600000"],
    );
    let theirs = run(
        &["--rm", "--network", "container:j3", "--pid", "container:j3"],
        &["ps"],
    );
    assert!(theirs.stdout.contains("testguest sleep"), "{theirs}");
    // j1's end is its namespace's: j2's command goes with it, killed.
    assert_eq!(shards(&["stop", "-t", "1", "j1"]).status, Some(0));
    let state = || shards(&["inspect", "-f", "{{.State.Status}} {{.State.ExitCode}}", "j2"]).stdout;
    let deadline = Instant::now() + TIMEOUT;
    while state() != "exited 137\n" {
        assert!(
            Instant::now() < deadline,
            "j2 outlived its namespace: {}",
            state()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(shards(&["stop", "-t", "1", "j3"]).status, Some(0));
    assert_eq!(shards(&["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
}

/// A joiner under `--init`, in a PID namespace of its own, has a reaper first in it, as the
/// workload has (D115, D119), docker-init's part: its command the namespace's second
/// process, so that `stop`'s SIGTERM ends at once a command that does not handle it; the
/// reaper as the command's user, which the joiner's own image names; `top` lists both.
#[test]
fn a_joiner_under_init_has_a_reaper_of_its_own() {
    let Some((home, image)) = home("containers-joiner-init") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let joined = run_in(
        &home,
        &image,
        &["-d", "--name", "joiner", "--network", "container:prov", "--init"],
        &["sleep"],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    let read = |path: &str| {
        let r = shards(&["exec", "joiner", "/bin/testguest", "fs", &format!("print:{path}")]);
        assert_eq!(r.status, Some(0), "{path}: {r}");
        r.stdout
    };
    assert_eq!(read("/proc/2/cmdline"), "/bin/testguest\0sleep\0");
    assert!(!read("/proc/1/cmdline").starts_with("/bin/testguest"));
    // The image's user, app.
    let status = read("/proc/1/status");
    assert!(
        status
            .lines()
            .any(|l| l.split_whitespace().eq(["Uid:", "1000", "1000", "1000", "1000"])),
        "{status}"
    );
    let top = shards(&["top", "joiner", "-o", "pid,args"]);
    assert_eq!(top.stdout.lines().count(), 3, "{top}");
    let stopping = Instant::now();
    assert_eq!(shards(&["stop", "joiner"]).status, Some(0));
    assert!(
        stopping.elapsed() < Duration::from_secs(5),
        "{:?}",
        stopping.elapsed()
    );
    let code = shards(&["inspect", "-f", "{{.State.ExitCode}}", "joiner"]);
    assert_eq!(code.stdout, "143\n", "{code}");
    assert_eq!(shards(&["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
}

/// A joiner takes devices as the workload does (D44, D119): the VM's, at another path,
/// with the access given; its cgroup's rules; and with `--privileged` every device the
/// microVM has, as a privileged Docker container has its host's. Without them, Docker's
/// default rules confine it, as they confine the workload.
#[test]
fn a_joiner_takes_devices_as_a_container_does() {
    let Some((home, image)) = home("containers-joiner-devices") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let fs = |opts: &[&str], ops: &[&str]| {
        let mut o = vec!["--rm", "--network", "container:prov"];
        o.extend_from_slice(opts);
        let mut a = vec!["fs"];
        a.extend_from_slice(ops);
        run_in(&home, &image, &o, &a)
    };
    let r = fs(&["-u", "0"], &["mknod:b:259:0:/disk", "open:r:/disk"]);
    assert!(r.stderr.contains("open:r:/disk: Operation not permitted"), "{r}");
    let r = fs(
        &["--device", "/dev/fuse:/dev/x:r"],
        &["dev:/dev/x", "open:r:/dev/x", "open:w:/dev/x"],
    );
    assert!(r.stdout.contains("/dev/x c 10:229 666"), "{r}");
    assert!(r.stderr.contains("open:w:/dev/x: Operation not permitted"), "{r}");
    let r = fs(
        &["-u", "0", "--device-cgroup-rule", "b 259:0 r"],
        &["mknod:b:259:0:/disk", "open:r:/disk"],
    );
    assert_eq!(r.status, Some(0), "{r}");
    let r = fs(&["-u", "0", "--privileged"], &["open:r:/dev/pmem0"]);
    assert_eq!(r.status, Some(0), "{r}");
    assert_eq!(shards_in(&home, &["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
}

/// A joiner keeps its writable layer as it stops, as any container does (D37, D119): its
/// changes are read while it is stopped, a file is copied into it then, and all of them
/// are put back over its image as it starts again in its provider's microVM, its names
/// its provider's still (dockerd checks what it was created by as it is created alone).
#[test]
fn a_joiner_keeps_its_files_as_it_stops() {
    let Some((home, image)) = home("containers-joiner-kept") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let detached = run_in(
        &home,
        &image,
        &["-d", "--name", "joiner", "--network", "container:prov"],
        &["sleep"],
    );
    assert_eq!(detached.status, Some(0), "{detached}");
    let wrote = shards(&[
        "exec",
        "-u",
        "0",
        "joiner",
        "/bin/testguest",
        "fs",
        "write:/made=kept",
        "rm:/etc/group",
    ]);
    assert_eq!(wrote.status, Some(0), "{wrote}");
    assert_eq!(shards(&["stop", "-t", "1", "joiner"]).status, Some(0));
    // Stopped, its changes are read in a VM over its files, as any container's.
    let changes = "C /etc\nD /etc/group\nA /made\n";
    let diff = shards(&["diff", "joiner"]);
    assert_eq!((diff.status, diff.stdout.as_str()), (Some(0), changes), "{diff}");
    let note = home.join("note");
    std::fs::write(&note, "copied").unwrap();
    let into = shards(&["cp", note.to_str().unwrap(), "joiner:/note"]);
    assert_eq!((into.status, into.stderr.as_str()), (Some(0), ""), "{into}");
    // Started again, in its provider's microVM again, over what it changed.
    let started = shards(&["start", "joiner"]);
    assert_eq!(
        (started.status, started.stdout.as_str()),
        (Some(0), "joiner\n"),
        "{started}"
    );
    let read = shards(&[
        "exec",
        "joiner",
        "/bin/testguest",
        "fs",
        "print:/made",
        "print:/note",
    ]);
    assert_eq!(
        (read.status, read.stdout.as_str()),
        (Some(0), "keptcopied"),
        "{read}"
    );
    let diff = shards(&["diff", "joiner"]);
    assert_eq!(
        (diff.status, diff.stdout.as_str()),
        (Some(0), "C /etc\nD /etc/group\nA /made\nA /note\n"),
        "{diff}"
    );
    let names = |name: &str| shards(&["inspect", "-f", "{{.Config.Hostname}}", name]).stdout;
    assert_eq!(names("joiner"), names("prov"));
    assert_eq!(shards(&["stop", "-t", "1", "joiner"]).status, Some(0));
    assert_eq!(shards(&["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
}

/// A joiner's writable layer is kept whole though its provider's command ends as it is
/// sent (D119): init goes on sending it as it ends the provider's other processes. Killed
/// with them, its sender left the host waiting for the rest of the layer, and the
/// provider's run never ended (a `--use-api-socket` run's member, on x86_64 Linux).
#[test]
fn a_joiners_layer_is_kept_whole_as_its_provider_ends() {
    let Some((home, image)) = home("containers-joiner-ends") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let provider = run_in(&home, &image, &["-d", "--name", "prov"], &["sleep"]);
    assert_eq!(provider.status, Some(0), "{provider}");
    // A layer whose sending outlasts the provider's kill, which follows its end.
    let joined = run_in(
        &home,
        &image,
        &["--name", "joiner", "--network", "container:prov", "-u", "0"],
        &["big", "/big", "256"],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    let killed = shards(&["kill", "prov"]);
    assert_eq!(killed.status, Some(0), "{killed}");
    let waited = shards(&["wait", "prov"]);
    assert_eq!(
        (waited.status, waited.stdout.as_str()),
        (Some(0), "137\n"),
        "{waited}"
    );
    let diff = shards(&["diff", "joiner"]);
    assert_eq!(
        (diff.status, diff.stdout.as_str()),
        (Some(0), "A /big\n"),
        "{diff}"
    );
}

/// A joiner's `--sysctl` is set in its own namespaces, as runc sets a container's from
/// within them (D119): an IPC one in its own IPC namespace, its provider's untouched; a
/// network one in the network namespace it shares, which Docker's joiner may set too.
#[test]
fn a_joiners_sysctls_are_set_in_its_namespaces() {
    let Some((home, image)) = home("containers-joiner-sysctls") else {
        return;
    };
    let mut provider = start(&home, &image, &["--name", "prov"], &["sleep"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    let (shm, port) = (
        "print:/proc/sys/kernel/shmmax",
        "print:/proc/sys/net/ipv4/ip_unprivileged_port_start",
    );
    let joined = run_in(
        &home,
        &image,
        &[
            "--rm",
            "--network",
            "container:prov",
            "--sysctl",
            "kernel.shmmax=1048576",
            "--sysctl",
            "net.ipv4.ip_unprivileged_port_start=500",
        ],
        &["fs", shm, port],
    );
    assert_eq!(
        (joined.status, joined.stdout.as_str()),
        (Some(0), "1048576\n500\n"),
        "{joined}"
    );
    let theirs = shards(&["exec", "prov", "/bin/testguest", "fs", shm, port]);
    assert_eq!(theirs.status, Some(0), "{theirs}");
    let mut lines = theirs.stdout.lines();
    assert_ne!(lines.next(), Some("1048576"), "{theirs}");
    assert_eq!(lines.next(), Some("500"), "{theirs}");
    assert_eq!(shards(&["stop", "-t", "1", "prov"]).status, Some(0));
    let _ = exit(&mut provider);
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
    assert_eq!(again.stderr, "shards: No such container: web\n");
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
        taken.stderr.contains("--name one: the name is in use by microVM"),
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

/// The command hears stop's SIGTERM and dies of it, as under `docker run --init`, where it
/// is not PID 1, which hears only what it handles (`a_command_is_pid_1_as_a_containers_is`).
#[test]
fn stop_and_kill_signal_the_command() {
    let Some((home, image)) = home("containers-stop") else {
        return;
    };
    let mut sleeper = start(&home, &image, &["--name", "sleeper", "--init"], &["sleep"]);
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
    // Each row: its name, its share of a CPU, and its memory against the microVM's.
    let row = |stats: &Run, name: &str| -> (f64, String) {
        let line = stats
            .stdout
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some(name))
            .unwrap();
        let fields: Vec<&str> = line.split_whitespace().collect();
        let cpu = fields[2].trim_end_matches('%').parse().unwrap();
        // Against the memory the microVM may have, no limit given, its host's, as
        // Docker's is its host's (D149: `128GiB` on a host of 128); its interfaces' and
        // devices' bytes, and its one process.
        assert_eq!(fields[4], "/", "{line}");
        let limit: f64 = fields[5]
            .strip_suffix("GiB")
            .and_then(|g| g.parse().ok())
            .unwrap_or(0.0);
        let host = common::host_memory() as f64;
        assert!(limit * f64::from(1u32 << 30) > host / 2.0, "{line}");
        assert_eq!(fields.last(), Some(&"1"), "{line}");
        (cpu, fields[3].to_string())
    };
    // A workload spinning one vCPU uses what the host gives that vCPU, which on a busy
    // host is less than a CPU (34% on a shared CI runner, 2026-10-07; 9.55% here as a gate's
    // tests ran beside it, 2026-10-17), and a second of a host at a load of 30 to 47 gave it
    // none (0.00%, a gate, 2026-10-19); one asleep, almost nothing, as little as the column
    // shows (0.00%, its two decimals). Measured from each guest, the two stand apart by an
    // order of magnitude, the sleeper's taken as no less than the column's least, in a table
    // asked for until they do, for a minute at most: what the host gives a second is not
    // what is tested.
    let deadline = Instant::now() + Duration::from_secs(60);
    let (stats, spinning, sleeping, held) = loop {
        let stats = shards_in(&home, &["stats", "--no-stream"]);
        assert_eq!(stats.status, Some(0), "{stats}");
        // Docker's columns, in its order: each as wide as its widest cell and three spaces
        // more (its tabwriter's padding), so a reading wider than its header, as `1016KiB /
        // 15.58GiB` is than `MEM USAGE / LIMIT`, widens the header's spaces too.
        let header = stats.stdout.lines().next().unwrap_or_default();
        let columns: Vec<&str> = header
            .split("   ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        assert_eq!(
            columns,
            [
                "CONTAINER ID",
                "NAME",
                "CPU %",
                "MEM USAGE / LIMIT",
                "MEM %",
                "NET I/O",
                "BLOCK I/O",
                "PIDS"
            ],
            "{stats}"
        );
        let (spinning, held) = row(&stats, "spinner");
        let (sleeping, _) = row(&stats, "sleeper");
        if spinning > 10.0 * sleeping.max(0.01) || Instant::now() > deadline {
            break (stats, spinning, sleeping, held);
        }
    };
    assert!(spinning > 10.0 * sleeping.max(0.01) && sleeping < 10.0, "{stats}");
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
        (Some(1), "shards: No such container: nobody\n"),
        "{missing}"
    );
    for name in ["spinner", "sleeper"] {
        assert_eq!(shards_in(&home, &["rm", "-f", name]).status, Some(0));
    }
    exit(&mut spinner);
    exit(&mut sleeper);
}

/// alpine:3.22 as Docker 29.3.1 ran it for D115's measurements (platform-measurements.md
/// M145): its shell and ps say what a command sees of its PID namespace.
const ALPINE: &str = "alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8";

/// A home with `image` pulled from its registry.
fn pulled(name: &str, image: &str) -> Option<TempDir> {
    if cannot_run_vms() {
        return None;
    }
    let home = TempDir::new(name);
    let pulled = shards_in(&home, &["pull", "-q", image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    Some(home)
}

/// A run's command is PID 1 of a PID namespace of its own, as a container's is (D115), with
/// PID 1's signals; `--init` puts a reaper there in docker-init's place, which forwards
/// what it hears; execs and restarts join or begin such a namespace as Docker's do;
/// `--pid host` is the microVM's own. Each answer is Docker 29.3.1's in shards-dind
/// (M145) but for what runc's own threads and docker-init make other there: the PID an
/// exec or `--init`'s command gets (Docker's 7), the reaper's name, and the PPID of
/// `--init`'s command, which is 0 here, its parent being init, outside the namespace.
#[test]
fn a_command_is_pid_1_as_a_containers_is() {
    let Some(home) = pulled("containers-pid1", ALPINE) else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = |args: &[&str]| {
        let mut all = vec!["run", "--rm"];
        all.extend_from_slice(args);
        shards(&all)
    };
    let said = |r: &Run| (r.status, r.stdout.clone());
    let r = run(&[ALPINE, "sh", "-c", "echo $$"]);
    assert_eq!(said(&r), (Some(0), "1\n".into()), "{r}");
    // From inside, PID 1 hears only what it handles: TERM and KILL alike go unheard.
    let r = run(&[ALPINE, "sh", "-c", "kill -TERM 1; kill -KILL 1; echo alive"]);
    assert_eq!(said(&r), (Some(0), "alive\n".into()), "{r}");
    let r = run(&[ALPINE, "ps", "-o", "pid,ppid,comm"]);
    assert_eq!(
        said(&r),
        (Some(0), "PID   PPID  COMMAND\n    1     0 ps\n".into()),
        "{r}"
    );
    let r = run(&["--init", ALPINE, "ps", "-o", "pid,ppid,comm"]);
    assert_eq!(
        said(&r),
        (
            Some(0),
            "PID   PPID  COMMAND\n    1     0 init\n    2     0 ps\n".into()
        ),
        "{r}"
    );
    // The reaper forwards what PID 1 hears to the command, as tini does, of any user: it
    // runs as the command's, as tini does.
    for user in ["0", "1000"] {
        let r = run(&[
            "--init",
            "-u",
            user,
            ALPINE,
            "sh",
            "-c",
            "kill -TERM 1; sleep 5; echo alive",
        ]);
        assert_eq!(said(&r), (Some(143), String::new()), "-u {user}: {r}");
    }

    // A stop: PID 1 without a handler waits out the timeout, then is killed; under --init
    // the command hears SIGTERM at once; with a handler, it ends as that says.
    let code = |name: &str| shards(&["inspect", "-f", "{{.State.ExitCode}}", name]).stdout;
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    for (name, flags, command, ready, timeout, quick, exit) in [
        ("pid1", &[][..], &["sleep", "1000"][..], None, "2", false, "137\n"),
        (
            "reaped",
            &["--init"][..],
            &["sleep", "1000"][..],
            None,
            "10",
            true,
            "143\n",
        ),
        (
            "trapped",
            &[][..],
            &[
                "sh",
                "-c",
                "trap 'exit 7' TERM; echo trapping; while :; do sleep 0.1; done",
            ][..],
            Some("trapping"),
            "10",
            true,
            "7\n",
        ),
    ] {
        let mut args = vec!["run", "-d", "--name", name];
        args.extend_from_slice(flags);
        args.push(ALPINE);
        args.extend_from_slice(command);
        let started = shards(&args);
        assert_eq!(started.status, Some(0), "{started}");
        // Its handler set before it is stopped: PID 1 of a namespace discards a signal it
        // neither handles nor blocks (kernel/signal.c, sig_task_ignored), as a container's
        // does under Docker, and a shell may not have run `trap` as `run -d` returns.
        if let Some(ready) = ready {
            let deadline = Instant::now() + TIMEOUT;
            while !shards(&["logs", name]).stdout.contains(ready) {
                assert!(Instant::now() < deadline, "{name} never said {ready:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let at = Instant::now();
        let stopped = shards(&["stop", "-t", timeout, name]);
        let took = at.elapsed();
        assert_eq!(stopped.status, Some(0), "{stopped}");
        if quick {
            assert!(took < Duration::from_secs(5), "{name} took {took:?} to stop");
        } else {
            assert!(
                took >= Duration::from_secs(2),
                "{name} stopped in {took:?}: it heard SIGTERM"
            );
        }
        assert_eq!(code(name), exit, "{name}");
    }
    // Its events, as dockerd's (PM M145): the SIGTERM it did not hear, then the SIGKILL that
    // ended it, and its death of that.
    let until = format!(
        "{:.9}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    );
    let events = shards(&[
        "events",
        "--since",
        &since,
        "--until",
        &until,
        "--filter",
        "container=pid1",
    ]);
    let told: Vec<(&str, &str)> = events
        .stdout
        .lines()
        .filter_map(|l| {
            let action = l.split(' ').nth(2)?;
            let attributes = l.get(l.find('(')? + 1..l.rfind(')')?)?;
            let said = attributes
                .split(", ")
                .find(|kv| kv.starts_with("signal=") || kv.starts_with("exitCode="))?;
            Some((action, said))
        })
        .collect();
    assert_eq!(
        told,
        [
            ("kill", "signal=15"),
            ("kill", "signal=9"),
            ("die", "exitCode=137")
        ],
        "{events}"
    );
    assert!(
        events.stdout.lines().any(|l| l.split(' ').nth(2) == Some("stop")),
        "{events}"
    );
    // `kill -s TERM` leaves it running; `kill`, SIGKILL, which PID 1 hears from outside,
    // ends it.
    assert_eq!(
        shards(&["run", "-d", "--name", "deaf", ALPINE, "sleep", "1000"]).status,
        Some(0)
    );
    assert_eq!(shards(&["kill", "-s", "TERM", "deaf"]).status, Some(0));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        shards(&["inspect", "-f", "{{.State.Running}}", "deaf"]).stdout,
        "true\n"
    );
    // An exec joins its namespace: the command is PID 1 there.
    let exec = shards(&[
        "exec",
        "deaf",
        "sh",
        "-c",
        "echo $$; tr '\\0' ' ' </proc/1/cmdline",
    ]);
    let lines: Vec<&str> = exec.stdout.lines().collect();
    assert!(
        exec.status == Some(0) && lines.len() == 2 && lines[0] != "1" && lines[1] == "sleep 1000 ",
        "{exec}"
    );
    assert_eq!(shards(&["kill", "deaf"]).status, Some(0));
    assert_eq!(code("deaf"), "137\n");
    // `top` lists the reaper under --init, as `docker top` lists docker-init.
    assert_eq!(
        shards(&["run", "-d", "--init", "--name", "listed", ALPINE, "sleep", "1000"]).status,
        Some(0)
    );
    let top = shards(&["top", "listed", "-o", "pid,comm"]);
    let names: Vec<&str> = top
        .stdout
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    assert_eq!(names, ["init", "sleep"], "{top}");
    assert_eq!(shards(&["kill", "listed"]).status, Some(0));

    // A restart policy's every run: PID 1 again.
    let started = shards(&[
        "run",
        "-d",
        "--restart",
        "on-failure:2",
        "--name",
        "again",
        ALPINE,
        "sh",
        "-c",
        "echo $$; exit 3",
    ]);
    assert_eq!(started.status, Some(0), "{started}");
    let deadline = Instant::now() + TIMEOUT;
    let format = "{{.RestartCount}} {{.State.ExitCode}} {{.State.Status}}";
    while shards(&["inspect", "-f", format, "again"]).stdout != "2 3 exited\n" {
        assert!(Instant::now() < deadline, "again did not end its restarts");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(shards(&["logs", "again"]).stdout, "1\n1\n1\n");

    // `--pid host`: the microVM's own namespace, in which kthreadd is PID 2, and whose
    // init's root the command, without CAP_SYS_PTRACE, cannot reach.
    let r = run(&[
        "--pid",
        "host",
        ALPINE,
        "sh",
        "-c",
        "echo $$; cat /proc/2/comm; ls /proc/1/root >/dev/null 2>&1 && echo reached || echo denied",
    ]);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert!(
        r.status == Some(0) && lines.len() == 3 && lines[0] != "1" && lines[1..] == ["kthreadd", "denied"],
        "{r}"
    );
    // The rest, refused in shards' words.
    let r = run(&["--pid", "foo", ALPINE, "true"]);
    assert!(
        r.status == Some(125)
            && r.stderr
                .contains("--pid foo: give host (the microVM's PID namespace) or container:NAME"),
        "{r}"
    );
    let r = shards(&["create", "--pid", "container:nope", ALPINE, "true"]);
    assert_eq!(
        (r.status, r.stderr.as_str()),
        (Some(1), "shards: --pid container:nope: no run is named nope\n"),
        "{r}"
    );
    for (name, flags, want) in [
        ("i0", &[][..], "null \"\" unset\n"),
        ("i1", &["--init"][..], "true \"\" set\n"),
        ("i2", &["--init=false"][..], "false \"\" set\n"),
        ("i3", &["--pid", "host"][..], "null \"host\" unset\n"),
    ] {
        let mut args = vec!["create", "--name", name];
        args.extend_from_slice(flags);
        args.extend([ALPINE, "true"]);
        assert_eq!(shards(&args).status, Some(0));
        let format = "{{json .HostConfig.Init}} {{json .HostConfig.PidMode}} {{if .HostConfig.Init}}set{{else}}unset{{end}}";
        assert_eq!(shards(&["inspect", "-f", format, name]).stdout, want, "{name}");
    }
    // Another container's, a microVM of its own: none of its processes' namespaces is ours,
    // but a container's joining its network, in its microVM (D119).
    let r = shards(&["create", "--pid", "container:i0", ALPINE, "true"]);
    assert_eq!(
        (r.status, r.stderr.as_str()),
        (
            Some(1),
            "shards: --pid container:i0: a run shares another's PID namespace inside that run's microVM, which it joins with --network container:i0\n"
        ),
        "{r}"
    );
}

/// Docker's own sysctls for a container's network namespace (moby daemon/oci_linux.go),
/// which a run's `--sysctl` overrides: ICMP echo sockets for every group, so that ping
/// works without CAP_NET_RAW, and every port bindable without CAP_NET_BIND_SERVICE; as
/// Docker 29.3.1 answered (M145).
#[test]
fn default_sysctls_let_ping_go_without_net_raw() {
    let Some(home) = pulled("containers-sysctls", ALPINE) else {
        return;
    };
    let run = |args: &[&str]| {
        let mut all = vec!["run", "--rm"];
        all.extend_from_slice(args);
        shards_in(&home, &all)
    };
    let keys = [
        "sysctl",
        "net.ipv4.ping_group_range",
        "net.ipv4.ip_unprivileged_port_start",
    ];
    let mut args = vec![ALPINE];
    args.extend(keys);
    let r = run(&args);
    assert_eq!(
        (r.status, r.stdout.as_str()),
        (
            Some(0),
            "net.ipv4.ping_group_range = 0\t2147483647\nnet.ipv4.ip_unprivileged_port_start = 0\n"
        ),
        "{r}"
    );
    let mut args = vec!["--sysctl", "net.ipv4.ip_unprivileged_port_start=1024", ALPINE];
    args.extend(keys);
    let r = run(&args);
    assert!(
        r.stdout.ends_with("net.ipv4.ip_unprivileged_port_start = 1024\n"),
        "{r}"
    );
    let r = run(&[
        "--cap-drop",
        "NET_RAW",
        "-u",
        "1000",
        ALPINE,
        "ping",
        "-c",
        "1",
        "-W",
        "2",
        "127.0.0.1",
    ]);
    assert!(
        r.status == Some(0) && r.stdout.contains("1 packets received"),
        "{r}"
    );
}

/// linuxserver.io's Alpine base image, whose entrypoint, s6-overlay's `/init`, runs only
/// as PID 1, as Docker 29.3.1 ran it in shards-dind (M145): its services up and its
/// command run; under `--init` or `--pid host`, refused by s6 itself.
const S6: &str = "ghcr.io/linuxserver/baseimage-alpine:3.22@sha256:ab81abc99e45ef5b045a6c6f41fb2f3ac3651b89f81b0942e1d8861650605cb0";

#[test]
fn an_s6_overlay_image_runs_as_pid_1() {
    let Some(home) = pulled("containers-s6", S6) else {
        return;
    };
    let run = |args: &[&str]| {
        let mut all = vec!["run", "--rm"];
        all.extend_from_slice(args);
        all.extend([S6, "echo", "hello from s6"]);
        shards_in(&home, &all)
    };
    let r = run(&[]);
    assert!(
        r.status == Some(0) && r.stdout.contains("[ls.io-init] done.\nhello from s6\n"),
        "{r}"
    );
    for refused in [&["--init"][..], &["--pid", "host"][..]] {
        let r = run(refused);
        assert!(
            r.status == Some(100)
                && format!("{}{}", r.stdout, r.stderr)
                    .contains("s6-overlay-suexec: fatal: can only run as pid 1"),
            "{refused:?}: {r}"
        );
    }
}

/// A microVM's I/O limits (D178): its VMM paces what its guest writes to a volume, and
/// reads from one, at `--io-maxbandwidth`'s bytes and `--io-maxiops`'s requests a second,
/// which no guest lifts, and nothing without them; `inspect` says them as dockerd's
/// HostConfig does (D178); a container joining it, whose I/O is the microVM's, takes none
/// of its own.
#[test]
fn a_microvms_io_is_paced_at_its_limits() {
    let Some((home, image)) = home("containers-io") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    // The guest's own count of how long its writes took, all synced.
    let poured = |limits: &[&str], bytes: &str, chunk: &str| -> u128 {
        let mut options = vec!["--rm", "-u", "0", "-v", "io:/v"];
        options.extend_from_slice(limits);
        let r = run_in(&home, &image, &options, &["pour", "/v/p", bytes, chunk]);
        assert_eq!(r.status, Some(0), "{limits:?}: {r}");
        let ms = r
            .stdout
            .strip_prefix(&format!("poured {bytes} in "))
            .and_then(|t| t.trim_end().strip_suffix(" ms"))
            .unwrap_or_else(|| panic!("{limits:?}: {}", r.stdout));
        ms.parse().unwrap()
    };
    let free = poured(&[], "3145728", "65536");
    // 3 MiB at 1 MiB a second: at least the two seconds after its first request; and longer
    // than with no limit wherever that took less, which a host busy enough to sync 48 writes
    // slower (4,555 ms with no limit, 4,406 paced, a gate's tests and others' builds beside
    // it) does not.
    let paced = poured(&["--io-maxbandwidth", "1m"], "3145728", "65536");
    assert!(
        paced >= 1900 && (free >= 1900 || paced > free),
        "free {free} ms, paced {paced} ms"
    );
    // Read back by a microVM of its own, which has none of it cached: paced too, by the
    // bytes each READ asks for.
    let read = |limits: &[&str]| -> u128 {
        let mut options = vec!["--rm", "-u", "0", "-v", "io:/v"];
        options.extend_from_slice(limits);
        let r = run_in(&home, &image, &options, &["read", "/v/p"]);
        assert_eq!(r.status, Some(0), "{limits:?}: {r}");
        let ms = r
            .stdout
            .strip_prefix("read /v/p 3145728 in ")
            .and_then(|t| t.trim_end().strip_suffix(" ms"))
            .unwrap_or_else(|| panic!("{limits:?}: {}", r.stdout));
        ms.parse().unwrap()
    };
    let unpaced = read(&[]);
    let held = read(&["--io-maxbandwidth", "1m"]);
    assert!(
        held >= 1900 && (unpaced >= 1900 || held > unpaced),
        "free {unpaced} ms, paced {held} ms"
    );
    // Thirty synced writes, each a WRITE and an FSYNC at least, at 20 requests a second.
    let counted = poured(&["--io-maxiops", "20"], "122880", "4096");
    assert!(counted >= 2900, "{counted} ms");
    let run = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "limited",
        "--io-maxbandwidth",
        "1m",
        "--io-maxiops",
        "20",
        &image,
        "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let inspected = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.IOMaximumBandwidth}} {{.HostConfig.IOMaximumIOps}}",
        "limited",
    ]);
    assert_eq!(inspected.stdout, "1048576 20\n", "{inspected}");
    let joined = shards(&[
        "run",
        "--rm",
        "--pull",
        "never",
        "--network",
        "container:limited",
        "--io-maxiops",
        "5",
        &image,
        "sleep",
    ]);
    assert_ne!(joined.status, Some(0), "{joined}");
    assert!(
        joined
            .stderr
            .contains("a joining container's I/O is that microVM's"),
        "{joined}"
    );
    assert_eq!(shards(&["rm", "-f", "limited"]).status, Some(0));
}

/// A volume's large files go through its microVM's DAX window where the daemon gives
/// one (`SHARDS_SHARE_WINDOW_MIB`, D189): its guest maps them from the host's page cache,
/// as statx says, and reads its small ones through FUSE as before; it reads the host's
/// bytes and keeps none of them in its own page cache; what it writes in place the host
/// reads, and what the host writes it reads, in ranges it maps to read and to write; a
/// file grown past its end, by either, it reads whole; and one the host cuts short under
/// its mapping leaves it zeros where the host has no page, never an end to its microVM,
/// and the file once the host writes there again; whole pages it writes where its host
/// holds none of them, the host and the guest read (as FUSE_WRITEs, patch 0009).
// Host directories and their DAX windows on Windows come with the platform parity plan's step 6.
#[cfg(unix)]
#[test]
fn a_volumes_large_files_go_through_its_dax_window() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("containers-dax");
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
    let dir = TempDir::new("containers-dax-v");
    // 8 MiB, four of the window's 2 MiB ranges, none of whose bytes repeat a range's.
    let big: Vec<u8> = (0..8u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(dir.join("big"), &big).unwrap();
    std::fs::write(dir.join("small"), b"small").unwrap();
    let volume = format!("{}:/v", dir.display());
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "dax", "-u", "0", "-v", &volume, &image, "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let exec = |args: &[&str]| {
        let mut all = vec!["exec", "dax", "/bin/testguest"];
        all.extend_from_slice(args);
        let r = shards(&all);
        assert_eq!(r.status, Some(0), "{args:?}: {r}");
        r.stdout
    };
    let said = exec(&["fs", "dax:/v/big", "dax:/v/small"]);
    assert_eq!(said, "dax /v/big yes\ndax /v/small no\n");
    // The guest's page cache, before and after it reads the whole file: a read through
    // FUSE keeps all 8 MiB there, a read through the window none; it may grow by less
    // than half.
    let cached = || -> u64 {
        let line = exec(&["meminfo", "Cached"]);
        line.split_whitespace()
            .nth(1)
            .and_then(|k| k.parse().ok())
            .unwrap_or_else(|| panic!("{line}"))
    };
    let before = cached();
    let digest = format!("{:016x}", shards_testguest::fnv1a(&big));
    assert_eq!(exec(&["digest", "/v/big"]), format!("digest /v/big {digest}\n"));
    let after = cached();
    let half_kib = (big.len() as u64 >> 10) / 2;
    assert!(
        after.saturating_sub(before) < half_kib,
        "Cached {before} kB, then {after} kB"
    );
    // Written in place by the guest, through a handle opened to write alone, which the
    // window maps writable: the host reads it.
    let at = 5 << 20;
    let byte = big[at].wrapping_add(1);
    exec(&["fs", &format!("poke:/v/big:{at}:{byte}")]);
    let host = std::fs::read(dir.join("big")).unwrap();
    assert_eq!(host[at], byte);
    // Written by the host, in place, in a range the guest mapped to read and in the one it
    // mapped to write: the guest reads each through the mapping it holds.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("big"))
        .unwrap();
    for at in [3 << 20, (5 << 20) + 1] {
        use std::os::unix::fs::FileExt as _;
        let byte = big[at].wrapping_add(2);
        file.write_all_at(&[byte], at as u64).unwrap();
        assert_eq!(
            exec(&["fs", &format!("peek:/v/big:{at}")]),
            format!("peek /v/big {at} {byte}\n")
        );
    }
    // A file whose end is no range's: its last range is mapped past its end, where the
    // guest reads nothing. Grown past it, by the guest through FUSE and by the host, within
    // a range the guest maps, the guest reads through the window what each wrote.
    let odd: Vec<u8> = (0..(3u32 << 20) + 5000)
        .map(|i| (i.wrapping_mul(40_503) >> 8) as u8)
        .collect();
    std::fs::write(dir.join("odd"), &odd).unwrap();
    let digest = format!("{:016x}", shards_testguest::fnv1a(&odd));
    assert_eq!(exec(&["fs", "dax:/v/odd"]), "dax /v/odd yes\n");
    assert_eq!(exec(&["digest", "/v/odd"]), format!("digest /v/odd {digest}\n"));
    let end = odd.len();
    // At its end, in the page that holds it, and a page and more past it.
    for at in [end, end + 20_000] {
        exec(&["fs", &format!("poke:/v/odd:{at}:7")]);
        assert_eq!(
            exec(&["fs", &format!("peek:/v/odd:{at}")]),
            format!("peek /v/odd {at} 7\n")
        );
    }
    // The host's, which the guest reads once it asks the file's size again (VALID_S).
    let at = end + 40_000;
    {
        use std::os::unix::fs::FileExt as _;
        let odd_file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("odd"))
            .unwrap();
        odd_file.write_all_at(&[9], at as u64).unwrap();
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let r = shards(&[
            "exec",
            "dax",
            "/bin/testguest",
            "fs",
            &format!("peek:/v/odd:{at}"),
        ]);
        if r.status == Some(0) {
            assert_eq!(r.stdout, format!("peek /v/odd {at} 9\n"));
            break;
        }
        assert!(Instant::now() < deadline, "{r}");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Cut short by the host under the guest's mapping: read there before the guest asks
    // its size again, its window gives zeros past the file's end, where the host has no
    // page to give, and the microVM runs on (D189); after, the guest is at the file's end.
    // Written there again by the host, the guest reads the file's byte through the window.
    let odd_file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("odd"))
        .unwrap();
    odd_file.set_len(1 << 20).unwrap();
    let cut = shards(&["exec", "dax", "/bin/testguest", "fs", "peek:/v/odd:3145728"]);
    assert!(
        cut.stdout == "peek /v/odd 3145728 0\n"
            || (cut.status == Some(1) && cut.stderr.contains("failed to fill whole buffer")),
        "{cut}"
    );
    {
        use std::os::unix::fs::FileExt as _;
        odd_file.write_all_at(&[5], 3 << 20).unwrap();
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let r = shards(&["exec", "dax", "/bin/testguest", "fs", "peek:/v/odd:3145728"]);
        if r.stdout == "peek /v/odd 3145728 5\n" {
            break;
        }
        assert!(
            Instant::now() < deadline
                && (r.stdout == "peek /v/odd 3145728 0\n"
                    || r.stderr.contains("failed to fill whole buffer")),
            "{r}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Cut short by the host while a process holds it open, read whole: with nothing asked
    // about the file between, the holder's read past the new end goes through the mapping
    // it holds, and reads zeros (Linux: the daemon's watch told the VM, PM M273; HVF: the
    // read faults), its vCPU never spinning.
    std::fs::write(dir.join("held"), &odd).unwrap();
    let holder = {
        let home = home.to_path_buf();
        std::thread::spawn(move || {
            let env: [(&str, &OsStr); 4] = [
                ("SHARDS_HOME", home.as_os_str()),
                ("SHARDS_KERNEL", kernel.as_os_str()),
                ("SHARDS_INIT", init.as_os_str()),
                ("SHARDS_SHARE_WINDOW_MIB", "64".as_ref()),
            ];
            let hold = [
                "exec",
                "dax",
                "/bin/testguest",
                "fs",
                "hold:/v/held:3145728:/v/held-go",
            ];
            run_shards_env(&[], &hold, &env, TIMEOUT)
        })
    };
    let deadline = Instant::now() + TIMEOUT;
    while !dir.join("held-go.ready").exists() {
        assert!(Instant::now() < deadline, "the holder never read the file");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("held"))
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    std::fs::write(dir.join("held-go"), b"").unwrap();
    let held = holder.join().unwrap();
    assert_eq!(held.stdout, "hold /v/held 3145728 0\n", "{held}");
    // Whole pages written by the guest where its host holds none of the file (D189, patch
    // 0009): on Linux the host lets go of the file's pages first, and the guest's kernel
    // writes them as a FUSE_WRITE, by its window's record; elsewhere through the window.
    // Either way the host reads them, and the guest reads them through its window.
    let pages: Vec<u8> = (0..4u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(dir.join("pages"), &pages).unwrap();
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::open(dir.join("pages")).unwrap();
        file.sync_all().unwrap();
        // SAFETY: posix_fadvise(2) on a descriptor this test holds open.
        let advised = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(advised, 0);
    }
    assert_eq!(exec(&["fs", "dax:/v/pages"]), "dax /v/pages yes\n");
    let at = (2 << 20) + 3 * 4096;
    exec(&["fs", &format!("pages:/v/pages:{at}:4:7")]);
    let host = std::fs::read(dir.join("pages")).unwrap();
    assert!(host[at..at + 4 * 4096].iter().all(|&b| b == 7));
    assert_eq!(
        (host[at - 1], host[at + 4 * 4096]),
        (pages[at - 1], pages[at + 4 * 4096])
    );
    for off in [at, at + 4 * 4096 - 1] {
        assert_eq!(
            exec(&["fs", &format!("peek:/v/pages:{off}")]),
            format!("peek /v/pages {off} 7\n")
        );
    }
    assert_eq!(shards(&["rm", "-f", "dax"]).status, Some(0));
    // Mapped 2 MiB at a time by a process as the host grows it, then stat'ed and opened
    // again: the guest learns the new size, and lets go of none of the entries the process
    // still maps, which Linux's FUSE did at both (PM M274): the device keeps them at the
    // open, shards' kernel at the size (resources/kernel/patches/0007). No warning of
    // fs/dax.c in the guest's log, which a privileged microVM's exec reads (syslog(2)'s
    // CAP_SYSLOG is the first user namespace's).
    std::fs::write(dir.join("mapped"), [&big[..], &big[..1 << 20]].concat()).unwrap();
    let run = shards(&[
        "run",
        "-d",
        "--pull",
        "never",
        "--name",
        "dax2m",
        "--privileged",
        "-u",
        "0",
        "-v",
        &volume,
        &image,
        "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let mapper = {
        let home = home.to_path_buf();
        std::thread::spawn(move || {
            let env: [(&str, &OsStr); 4] = [
                ("SHARDS_HOME", home.as_os_str()),
                ("SHARDS_KERNEL", kernel.as_os_str()),
                ("SHARDS_INIT", init.as_os_str()),
                ("SHARDS_SHARE_WINDOW_MIB", "64".as_ref()),
            ];
            let map = [
                "exec",
                "dax2m",
                "/bin/testguest",
                "fs",
                "map2m:/v/mapped:/v/mapped-go",
            ];
            run_shards_env(&[], &map, &env, TIMEOUT)
        })
    };
    let deadline = Instant::now() + TIMEOUT;
    while !dir.join("mapped-go.ready").exists() {
        assert!(Instant::now() < deadline, "the mapper never mapped the file");
        std::thread::sleep(Duration::from_millis(10));
    }
    {
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("mapped"))
            .unwrap();
        file.write_all_at(b"x", 9 << 20).unwrap();
    }
    std::fs::write(dir.join("mapped-go"), b"").unwrap();
    let mapped = mapper.join().unwrap();
    assert_eq!(
        mapped.stdout,
        format!("map2m /v/mapped {}\n", (9 << 20) + 1),
        "{mapped}"
    );
    let log = shards(&["exec", "dax2m", "/bin/testguest", "fs", "kmsg:fs/dax.c"]);
    assert_eq!(log.status, Some(0), "{log}");
    assert!(log.stdout.ends_with("kmsg fs/dax.c 0\n"), "{log}");
    assert_eq!(shards(&["rm", "-f", "dax2m"]).status, Some(0));
}

/// A volume's files are mapped through its microVM's DAX window from `DAX_SMALLEST` bytes
/// (D189): a file of that size its guest maps, as statx says, and one a byte short it reads
/// through FUSE; each it reads whole as the host wrote it, what it writes in place the host
/// reads, and what the host writes in place it reads.
// Host directories and their DAX windows on Windows come with the platform parity plan's step 6.
#[cfg(unix)]
#[test]
fn a_volumes_files_are_mapped_from_dax_smallest_bytes() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("containers-daxs");
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
    let dir = TempDir::new("containers-daxs-v");
    let smallest = usize::try_from(shards_vmm::devices::virtio::fs::server::DAX_SMALLEST).unwrap();
    let bytes =
        |n: usize, by: u32| -> Vec<u8> { (0..n as u32).map(|i| (i.wrapping_mul(by) >> 24) as u8).collect() };
    let files = [
        ("at", bytes(smallest, 2_654_435_761)),
        ("short", bytes(smallest - 1, 40_503)),
    ];
    for (name, file) in &files {
        std::fs::write(dir.join(name), file).unwrap();
    }
    let volume = format!("{}:/v", dir.display());
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "daxs", "-u", "0", "-v", &volume, &image, "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let exec = |args: &[&str]| {
        let mut all = vec!["exec", "daxs", "/bin/testguest"];
        all.extend_from_slice(args);
        let r = shards(&all);
        assert_eq!(r.status, Some(0), "{args:?}: {r}");
        r.stdout
    };
    assert_eq!(
        exec(&["fs", "dax:/v/at", "dax:/v/short"]),
        "dax /v/at yes\ndax /v/short no\n"
    );
    for (name, file) in &files {
        let path = format!("/v/{name}");
        let digest = format!("{:016x}", shards_testguest::fnv1a(file));
        assert_eq!(exec(&["digest", &path]), format!("digest {path} {digest}\n"));
        // Its last byte written in place by the guest: the host reads it.
        let last = file.len() - 1;
        let byte = file[last].wrapping_add(1);
        exec(&["fs", &format!("poke:{path}:{last}:{byte}")]);
        assert_eq!(std::fs::read(dir.join(name)).unwrap()[last], byte, "{name}");
        // Its first written in place by the host: the guest reads it.
        let byte = file[0].wrapping_add(2);
        {
            use std::os::unix::fs::FileExt as _;
            let host = std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join(name))
                .unwrap();
            host.write_all_at(&[byte], 0).unwrap();
        }
        assert_eq!(
            exec(&["fs", &format!("peek:{path}:0")]),
            format!("peek {path} 0 {byte}\n")
        );
    }
    assert_eq!(shards(&["rm", "-f", "daxs"]).status, Some(0));
}

/// A volume's large files go through a DAX window by default, one as long as its host's
/// memory (D189, PM M306), but where the host is itself a virtual machine on an AMD CPU,
/// whose hypervisor maps each page a window gives anew (PM M308); through FUSE where
/// `SHARDS_SHARE_WINDOW_MIB` is 0; and through one where it gives one, on any host.
#[test]
fn a_volumes_large_files_go_through_a_dax_window_by_default() {
    if common::later_on_windows(6, "volumes' DAX windows") {
        return;
    }
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (kernel, init) = (kernel(), guest_init());
    let dir = TempDir::new("containers-daxdef-v");
    std::fs::write(dir.join("big"), vec![7u8; 4 << 20]).unwrap();
    let volume = format!("{}:/v", dir.display());
    let default = if host_is_a_virtual_machine_on_amd() {
        "no"
    } else {
        "yes"
    };
    for (window, dax) in [(None, default), (Some("0"), "no"), (Some("4096"), "yes")] {
        let home = TempDir::new("containers-daxdef");
        let mut env: Vec<(&str, &OsStr)> = vec![
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel.as_os_str()),
            ("SHARDS_INIT", init.as_os_str()),
        ];
        if let Some(mib) = window {
            env.push(("SHARDS_SHARE_WINDOW_MIB", mib.as_ref()));
        }
        let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
        let pulled = shards(&["pull", "-q", &image]);
        assert_eq!(pulled.status, Some(0), "{pulled}");
        let r = shards(&[
            "run",
            "--rm",
            "--pull",
            "never",
            "-u",
            "0",
            "-v",
            &volume,
            &image,
            "fs",
            "dax:/v/big",
        ]);
        assert_eq!(r.stdout, format!("dax /v/big {dax}\n"), "{window:?}: {r}");
        let _ = shards(&["stop", "daemon"]);
    }
}

/// Whether this host is itself a virtual machine on an AMD CPU, as Linux describes its
/// first processor (`/proc/cpuinfo`: AMD's vendor ID and the `hypervisor` flag): where a
/// share has no DAX window by default (D189, PM M308).
fn host_is_a_virtual_machine_on_amd() -> bool {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return false;
    }
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |key: &str| {
        info.lines().find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().to_string())
        })
    };
    field("vendor_id").as_deref() == Some("AuthenticAMD")
        && field("flags").is_some_and(|f| f.split_whitespace().any(|f| f == "hypervisor"))
}

/// A volume's DAX window costs its guest the ranges it maps, not every range it could: its
/// guest's FUSE makes a range's mapping (128 bytes, kmalloc-128) as it first wants the
/// range, not one for each 2 MiB of the window as the volume is mounted (D189, shards'
/// kernel patch 0013, PM M306). A microVM whose volume has a 64 GiB window holds, once its
/// command runs, fewer than half the 32,768 such a mount made more of them than one whose
/// volume has none.
#[test]
fn a_volumes_long_dax_window_takes_its_guest_no_mapping_for_each_range() {
    if common::later_on_windows(6, "volumes' DAX windows") {
        return;
    }
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (kernel, init) = (kernel(), guest_init());
    let dir = TempDir::new("containers-daxlong-v");
    let volume = format!("{}:/v", dir.display());
    let in_use = |window: &str| -> u64 {
        let home = TempDir::new("containers-daxlong");
        let env: [(&str, &OsStr); 4] = [
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel.as_os_str()),
            ("SHARDS_INIT", init.as_os_str()),
            ("SHARDS_SHARE_WINDOW_MIB", window.as_ref()),
        ];
        let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
        let pulled = shards(&["pull", "-q", &image]);
        assert_eq!(pulled.status, Some(0), "{pulled}");
        let r = shards(&[
            "run",
            "--rm",
            "--pull",
            "never",
            "-u",
            "0",
            "-v",
            &volume,
            &image,
            "slab",
            "kmalloc-128",
        ]);
        assert_eq!(r.status, Some(0), "{r}");
        let _ = shards(&["stop", "daemon"]);
        r.stdout
            .strip_prefix("slab kmalloc-128 ")
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or_else(|| panic!("no kmalloc-128 said: {r}"))
    };
    let none = in_use("0");
    let long = in_use("65536");
    assert!(
        long.saturating_sub(none) < 32_768 / 2,
        "kmalloc-128: {none} objects in use with no window, {long} with a 64 GiB one"
    );
}

/// A microVM's devices reach the pages of the files its guest maps, device memory, as they
/// reach its RAM (D189): a write from a mapping of its image's root (pmem) or of a file a
/// volume's DAX window maps, appended to a file the window maps, which its guest's kernel
/// gives its device as the mapping's pages, writes every byte; an O_DIRECT read of a file
/// under a range, which its guest reads through FUSE, into a mapping of a file the window
/// maps, or of one in its writable layer (pmem), reads every byte. Before, its device
/// answered the write as done with nothing written (0 bytes) and the read as done with its
/// bytes missing.
#[test]
fn a_microvms_devices_reach_the_pages_of_the_files_it_maps() {
    if common::later_on_windows(6, "volumes' DAX windows") {
        return;
    }
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("containers-dma");
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
    let dir = TempDir::new("containers-dma-v");
    let pattern =
        |len: u32, k: u32| -> Vec<u8> { (0..len).map(|i| (i.wrapping_mul(k) >> 8) as u8).collect() };
    let big = pattern(8 << 20, 40_503);
    let src = pattern(1 << 20, 2_654_435_761);
    std::fs::write(dir.join("big"), &big).unwrap();
    std::fs::write(dir.join("src"), &src).unwrap();
    for name in ["from-window", "from-root", "into"] {
        std::fs::write(dir.join(name), vec![0u8; 8 << 20]).unwrap();
    }
    let volume = format!("{}:/v", dir.display());
    let run = shards(&[
        "run", "-d", "--pull", "never", "--name", "dma", "-u", "0", "-v", &volume, &image, "sleep",
    ]);
    assert_eq!(run.status, Some(0), "{run}");
    let exec = |op: &str| {
        let r = shards(&["exec", "dma", "/bin/testguest", "fs", op]);
        assert_eq!(r.status, Some(0), "{op}: {r}");
        r.stdout
    };
    assert_eq!(
        exec("dax:/v/from-window") + &exec("dax:/bin/testguest"),
        "dax /v/from-window yes\ndax /bin/testguest yes\n"
    );
    assert_eq!(
        exec("frommap:/v/big:/v/from-window:append"),
        format!("frommap /v/from-window {}\n", 8 << 20)
    );
    let root = exec("frommap:/bin/testguest:/v/from-root:append");
    let wrote: u64 = root
        .strip_prefix("frommap /v/from-root ")
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("{root}"));
    assert!(wrote >= 1 << 20, "{root}");
    assert_eq!(
        exec("intomap:/v/src:/v/into"),
        format!("intomap /v/into {}\n", 1 << 20)
    );
    exec("write:/work/layer=x");
    exec("pages:/work/layer:0:256:0");
    assert_eq!(exec("dax:/work/layer"), "dax /work/layer yes\n");
    assert_eq!(
        exec("intomap:/v/src:/work/layer"),
        format!("intomap /work/layer {}\n", 1 << 20)
    );
    // What the host reads: each byte the guest moved.
    let appended = std::fs::read(dir.join("from-window")).unwrap();
    assert_eq!(appended.len(), 16 << 20);
    assert!(appended[8 << 20..] == big[..], "the window's bytes appended");
    assert_eq!(
        std::fs::metadata(dir.join("from-root")).unwrap().len(),
        (8 << 20) + wrote
    );
    let into = std::fs::read(dir.join("into")).unwrap();
    assert!(
        into[..1 << 20] == src[..],
        "the bytes read into the window's mapping"
    );
    assert_eq!(shards(&["rm", "-f", "dma"]).status, Some(0));
}

/// `--cgroup-parent` (D183): on Linux, a microVM's processes, its VM process and its
/// network process, are made under the slice of the user's systemd manager it names, by a
/// spawner the manager placed there once, the one process of a scope of the daemon's, and
/// are charged there from their first page; a second run under the slice is made in that
/// scope, and a run without one, its template's pool warm, in none of the slice; `inspect`
/// says it; a joiner names its provider's or none; and the scope goes with the daemon. A
/// path, which names no slice, is refused, as it is elsewhere than Linux, where a group
/// shards keeps is named (D183's slice 3, which its groups' tests hold).
#[test]
fn a_microvm_is_made_under_the_slice_its_cgroup_parent_names() {
    let Some((home, image)) = home("containers-parent") else {
        return;
    };
    // The image's template, and a pool of it warm in the daemon's own place: a run under a
    // slice takes none of its VMs.
    let plain = run_in(&home, &image, &["--rm"], &["exit", "0"]);
    assert_eq!(plain.status, Some(0), "{plain}");
    let path = run_in(
        &home,
        &image,
        &["--rm", "--cgroup-parent", "/agents"],
        &["exit", "0"],
    );
    assert_ne!(path.status, Some(0), "{path}");
    #[cfg(not(target_os = "linux"))]
    assert!(path.stderr.contains("letters, digits"), "{path}");
    #[cfg(target_os = "linux")]
    placed_under_slices(&home, &image, &path);
}

/// [`a_microvm_is_made_under_the_slice_its_cgroup_parent_names`] on Linux, whose groups are
/// its user's systemd manager's slices: a path refused as one.
#[cfg(target_os = "linux")]
fn placed_under_slices(home: &Path, image: &str, path: &Run) {
    use std::io::Write as _;
    let shards = |args: &[&str]| shards_in(home, args);
    let slice = format!("shardstest{}.slice", std::process::id());
    assert!(path.stderr.contains("NAME.slice"), "{path}");
    // The user's systemd manager, on its bus: CI's Linux runners have one, and must find it
    // (SHARDS_REQUIRE_USER_MANAGER).
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
        let _ = writeln!(
            std::io::stderr(),
            "SKIP: no user's systemd manager here: {} is not",
            bus.display()
        );
        return;
    }
    let started = run_in(
        home,
        image,
        &["-d", "--name", "placed", "--cgroup-parent", &slice],
        &["sleep"],
    );
    assert_eq!(started.status, Some(0), "{started}");
    let pid_of = |name: &str| -> u32 {
        let pid = shards(&["inspect", "-f", "{{.State.Pid}}", name]);
        pid.stdout.trim().parse().unwrap_or_else(|_| panic!("{pid}"))
    };
    let cgroup_of = |pid: u32| -> String {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        text.lines()
            .find_map(|l| l.strip_prefix("0::"))
            .unwrap()
            .to_string()
    };
    let vm = pid_of("placed");
    let scope = cgroup_of(vm);
    assert!(
        scope.contains(&format!("/{slice}/shards-")) && scope.ends_with(".scope"),
        "{scope}"
    );
    let scope_dir = Path::new("/sys/fs/cgroup").join(scope.trim_start_matches('/'));
    // In it: the VM process, its network process, and the spawner that made them, alone.
    let procs = std::fs::read_to_string(scope_dir.join("cgroup.procs")).unwrap();
    let mut names: Vec<String> = procs
        .lines()
        .map(|p| {
            std::fs::read_to_string(format!("/proc/{p}/comm"))
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["shards", "shards-net", "shards-vm"], "{procs}");
    // Charged there from its first page: all the VM process's own memory is the slice's, as
    // it would not be of a process moved there once it ran (cgroup-v2.rst, "Memory
    // Ownership").
    // Read again with each reading of the slice's: what the guest frees goes back to its
    // host as it goes (D151), so the VM's own is less by then (gnu x86_64 CI 39735519783:
    // 231,160 KiB as it was first read, 229,260 as the slice's 229,424 caught up).
    let own = || {
        std::fs::read_to_string(format!("/proc/{vm}/status"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("RssAnon:"))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap()
    };
    let slice_dir = scope_dir.parent().unwrap().to_path_buf();
    // Read until they have caught up: a cgroup's statistics are its descendants' as they were
    // last flushed into it (mm/memcontrol.c, memcg_vmstats_needs_flush and the periodic
    // flush_memcg_stats_dwork), where memory.current is charged as each page is: on Fedora
    // the slice's anon read 94.7 MB while its scope's read 264 MB and both its
    // memory.current 277 MB.
    let slice_anon = || {
        std::fs::read_to_string(slice_dir.join("memory.stat"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("anon "))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let (mut own_kib, mut charged) = (own(), slice_anon());
    while charged < own_kib << 10 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        own_kib = own();
        charged = slice_anon();
    }
    // What each cgroup holds, as a failure says it (gnu x86_64 CI 39273610992: none of it
    // the slice's).
    let held = |dir: &Path| {
        let stat = std::fs::read_to_string(dir.join("memory.stat")).unwrap_or_default();
        let kinds: Vec<&str> = stat
            .lines()
            .filter(|l| {
                ["anon ", "file ", "shmem ", "kernel "]
                    .iter()
                    .any(|k| l.starts_with(k))
            })
            .collect();
        format!(
            "{}: {}; memory.current {}",
            dir.display(),
            kinds.join(", "),
            std::fs::read_to_string(dir.join("memory.current"))
                .unwrap_or_default()
                .trim()
        )
    };
    let resident = std::fs::read_to_string(format!("/proc/{vm}/status"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("Rss"))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        own_kib > 0 && charged >= own_kib << 10,
        "the VM's own {own_kib} KiB, the slice's {charged} bytes\n{}\n{}\nthe VM's {resident}; its cgroup {scope}",
        held(&scope_dir),
        held(&slice_dir)
    );
    // A second run under it: made in the same scope, placed once.
    let second = run_in(
        home,
        image,
        &["-d", "--name", "second", "--cgroup-parent", &slice],
        &["sleep"],
    );
    assert_eq!(second.status, Some(0), "{second}");
    assert_eq!(cgroup_of(pid_of("second")), scope);
    // A run without one, its pool warm: none of the slice's.
    let free = run_in(home, image, &["-d", "--name", "free"], &["sleep"]);
    assert_eq!(free.status, Some(0), "{free}");
    let elsewhere = cgroup_of(pid_of("free"));
    assert!(!elsewhere.contains(&slice), "{elsewhere}");
    let inspected = shards(&["inspect", "-f", "{{.HostConfig.CgroupParent}}", "placed"]);
    assert_eq!(inspected.stdout, format!("{slice}\n"), "{inspected}");
    // A joiner runs in its provider's microVM, under its provider's parent alone.
    let other = format!("shardstest{}b.slice", std::process::id());
    let joined = run_in(
        home,
        image,
        &["--rm", "--network", "container:placed", "--cgroup-parent", &other],
        &["exit", "0"],
    );
    assert!(
        joined.status != Some(0) && joined.stderr.contains("runs in its microVM"),
        "{joined}"
    );
    let joined = run_in(
        home,
        image,
        &["--rm", "--network", "container:placed", "--cgroup-parent", &slice],
        &["exit", "0"],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    // The scope goes with the daemon, each of its processes ended.
    let stopped = shards(&["stop", "daemon"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let deadline = std::time::Instant::now() + TIMEOUT;
    while scope_dir.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} outlived the daemon",
            scope_dir.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // The slice is the test's, stopped as its user would.
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "stop", &slice])
        .status();
}

/// A microVM held to a budget short of what it holds gives back what it holds cold of its
/// files, and reads on within it (D188): under a slice of the user's manager, a microVM
/// that read 512 MiB through FUSE holds it in its guest's page cache (its daemon gives its
/// share no window, `SHARDS_SHARE_WINDOW_MIB` 0, on any host: a window maps files of
/// DAX_SMALLEST, a MiB, or more from its host's page cache, D189, and a host that gives a
/// window by default, an Intel runner's, left its guest nothing to give back); a `MemoryHigh` set
/// a quarter GiB short of what its slice holds then (memory.current), the daemon, watching
/// the slice's budget, has it reclaim what it holds cold and report it free at once, idle as
/// it is, which raises no pressure, says what it gave back and what it asked to unplug with
/// its budget's word, and holds it within the budget's room, in which it reads a gigabyte,
/// in files of a MiB, to the end. The budget from what the microVM holds, not a size: one of
/// 384 MiB held a microVM of fedora's 32 CPUs below its own footprint, its VM process
/// throttled until its give-back missed its 10 s. Linux alone, whose slices are its user's
/// manager's: macOS and Windows hold their microVMs to groups shards keeps, the group tests
/// below.
// Unix's: a slice of the user's manager, at the user's ID (getuid).
#[cfg(unix)]
#[test]
fn a_microvm_held_to_a_budget_gives_back_what_it_holds_cold() {
    if !cfg!(target_os = "linux") {
        return;
    }
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("containers-budget");
    let (kernel, init) = (kernel(), guest_init());
    // Its daemon, started by the pull, gives its share no window: the files come through
    // FUSE into the guest's page cache, which is what it holds cold.
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_SHARE_WINDOW_MIB", "0".as_ref()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
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
    let vol = TempDir::new("budget-vol");
    let files: Vec<String> = (0..1024).map(|i| format!("/vol/f{i}")).collect();
    for file in &files {
        let name = file.trim_start_matches("/vol/");
        std::fs::File::create(vol.join(name))
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
    }
    let slice = format!("shardsbudget{}.slice", std::process::id());
    /// The slice's budget taken off and the slice stopped, however the test ends.
    struct Budget<'a>(&'a str);
    impl Drop for Budget<'_> {
        fn drop(&mut self) {
            for verb in ["revert", "stop"] {
                let _ = Command::new("systemctl").args(["--user", verb, self.0]).status();
            }
        }
    }
    let _budget = Budget(&slice);
    let mount = format!("{}:/vol", vol.display());
    let args = run_args(
        &image,
        &["-d", "--name", "budget", "--cgroup-parent", &slice, "-v", &mount],
        &["sleep"],
    );
    let started = shards(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(started.status, Some(0), "{started}");
    let log = home.join("daemon.log");
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    let read = |files: &[String]| {
        let mut read = vec!["exec", "budget", "/bin/testguest", "read"];
        read.extend(files.iter().map(String::as_str));
        let read = shards(&read);
        assert_eq!(read.status, Some(0), "{read}\n{}", said());
        for file in files {
            assert!(
                read.stdout.contains(&format!("read {file} 1048576 ")),
                "{file}: {read}"
            );
        }
    };
    let set = |high: &str| {
        let set = Command::new("systemctl")
            .args(["--user", "set-property", "--runtime", &slice])
            .args([format!("MemoryHigh={high}").as_str(), "MemorySwapMax=0"])
            .output()
            .unwrap();
        assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));
    };
    // The daemon's first line past `at` in its log with `words` in it.
    let first = |at: usize, words: &[&str]| -> String {
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            let after = said().get(at..).unwrap_or_default().to_string();
            if let Some(line) = after.lines().find(|l| words.iter().any(|w| l.contains(w))) {
                return line.to_string();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "none of {words:?} said:\n{}",
                said()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mib_after = |line: &str, words: &str| -> u64 {
        line.split_once(words)
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no {words:?} in {line}"))
    };
    // What the slice holds: its microVM's VM process, its guest's page cache among it.
    let group = Command::new("systemctl")
        .args(["--user", "show", "-p", "ControlGroup", "--value", &slice])
        .output()
        .unwrap();
    let group = std::path::PathBuf::from(format!(
        "/sys/fs/cgroup{}",
        String::from_utf8_lossy(&group.stdout).trim()
    ));
    let current = || -> u64 {
        std::fs::read_to_string(group.join("memory.current"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    };
    // Half of them read with no budget, held in its guest's page cache; then a budget past
    // what the slice holds, whose first word says what the daemon counts it holding (what
    // its host may take back from its processes alone: not the page cache no process maps,
    // which memory.current counts, its share's own reads of the files among it, which held
    // a budget set a quarter GiB short of memory.current 242 MiB clear of it, CI
    // 40844212880); then the budget set a quarter GiB short of that, whose first word says
    // what it gave back. Its host's own pressure, which a busy CI runner meets, has
    // microVMs give back what they hold cold too, and where it took them first the microVM
    // was within its budget as it was set, with nothing to give back (CI 40715919365: 220
    // MiB given back at its host's pressure, then 190 MiB of room): the budget is let go of
    // and the files read again, a few times at most. Short of it with nothing given back
    // is no such race.
    let mut tries = 0;
    let gave = loop {
        read(files.get(..512).unwrap());
        // Its host's own pressure has it give back what it read as it reads it on a busy
        // host (Fedora beside a CI job: 231 MiB given back of 512 read, and its slice never
        // held them): read again until it holds them.
        let deadline = std::time::Instant::now() + TIMEOUT;
        while current() <= 512 << 20 {
            assert!(
                std::time::Instant::now() < deadline,
                "its slice never held what it read: {} bytes\n{}",
                current(),
                said()
            );
            std::thread::sleep(Duration::from_millis(50));
            read(files.get(..512).unwrap());
        }
        let before = said().len();
        set(&(current() + (1 << 30)).to_string());
        let counted = first(before, &["held within its budget", "budget is short by"]);
        let held = mib_after(&counted, "MiB, holds ");
        let before = said().len();
        set(&(held.saturating_sub(256) << 20).to_string());
        let word = first(before, &["held within its budget", "budget is short by"]);
        if word.contains("it gave back") && mib_after(&word, "warm VMs ended and ") > 0 {
            break word;
        }
        tries += 1;
        assert!(
            tries < 5 && word.contains("held within its budget"),
            "nothing given back said, try {tries}: {word}\n{}",
            said()
        );
        let unset = said().len();
        set("infinity");
        first(unset, &["held to no budget"]);
    };
    read(&files);
    // Held within the room its budget leaves, or, short of it, within the least room a run
    // goes on in: its host's taking back its template's pages as the budget was set counts
    // as no room (PM M318), and one that held 512 MiB of page cache under 384 MiB is short.
    assert!(
        said().contains("held within its budget") || said().contains("past what it uses, the least room"),
        "not said to be held within its budget:\n{}",
        said()
    );
    // What it asked to unplug of the memory it had plugged to hold them, said with what it
    // gave back of its files.
    let _said = mib_after(&gave, "asking to unplug ");
    assert_eq!(shards(&["kill", "budget"]).status, Some(0));
}

/// A group's `cpu.max` (D183, slice 3) where the host has no group of processes a user may
/// cap, macOS's and Windows': its microVMs hold themselves to it together, each VM process
/// charging the group's one count. Two microVMs under a group of one CPU, four threads
/// burning in each at once, use about one CPU's time over the burn; with the group's budget
/// `max` again, the same two use more, on a host that gives a program's eight threads more.
#[test]
fn a_groups_microvms_are_held_to_its_cpu_max_together() {
    if !cfg!(any(target_os = "macos", windows)) {
        return;
    }
    let Some((home, image)) = home("containers-group-cpu") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let host = std::thread::available_parallelism().unwrap().get();
    if host < 4 {
        eprintln!("SKIP: {host} CPUs, too few to show two microVMs held to one");
        return;
    }
    // Its budget written before its first microVM, as a user writes a cgroup's.
    let group = home.join("groups").join("cpu");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cpu.max"), "100000 100000\n").unwrap();
    let names = ["ga", "gb"];
    let runs: Vec<Child> = names
        .iter()
        .map(|name| {
            start(
                &home,
                &image,
                &["--name", name, "--cgroup-parent", "cpu"],
                &["sleep"],
            )
        })
        .collect();
    let pids: Vec<u32> = names
        .iter()
        .map(|name| {
            shards(&["inspect", "-f", "{{.State.Pid}}", name])
                .stdout
                .trim()
                .parse()
                .unwrap()
        })
        .collect();
    // Four threads for three seconds in each microVM at once: their VM processes' CPU time
    // over the burn, and its wall time.
    let burn = || -> (u64, u64) {
        let cpu = || {
            pids.iter()
                .map(|&pid| shards_vmm::platform::process_usage(pid).unwrap().cpu_ns)
                .sum::<u64>()
        };
        let (before, t0) = (cpu(), Instant::now());
        let burned: Vec<Run> = std::thread::scope(|s| {
            let each: Vec<_> = names
                .iter()
                .map(|name| s.spawn(move || shards(&["exec", name, "/bin/testguest", "burn", "4", "3000"])))
                .collect();
            each.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
        let used = cpu() - before;
        for b in &burned {
            assert!(b.stdout.ends_with("burned\n"), "{b}");
        }
        (used, wall)
    };
    let (used, wall) = burn();
    eprintln!("two microVMs under a cpu.max of one CPU: {used} ns of CPU in {wall} ns");
    // One CPU's time over the burn; the period's quota the group may run ahead; what each
    // microVM's four threads may run of its first period, quiet till then, before it is
    // held; and their VM processes' own threads, their devices'.
    assert!(
        used <= wall + 1_000_000_000,
        "{used} ns of CPU in {wall} ns, two microVMs held to one CPU together"
    );
    assert!(
        used >= wall / 4,
        "{used} ns of CPU in {wall} ns: held, and never let go"
    );
    // What the host gives eight busy threads of a program's now: a busy guest's vCPUs share
    // its CPUs as its programs do (D4, PM M317).
    let given: f64 = std::thread::scope(|s| {
        let spun: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    let clock = shards_vmm::platform::ThreadClock::current().unwrap();
                    let until = Instant::now() + Duration::from_secs(1);
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    clock.read().unwrap()
                })
            })
            .collect();
        spun.into_iter().map(|t| t.join().unwrap() as f64 / 1e9).sum()
    });
    // Its budget `max` again, heard by the daemon within a pressure window while the group
    // has microVMs.
    std::fs::write(group.join("cpu.max"), "max 100000\n").unwrap();
    std::thread::sleep(shards_vmm::platform::PRESSURE_WINDOW * 3);
    let (free, free_wall) = burn();
    eprintln!("the same two under a cpu.max of max: {free} ns of CPU in {free_wall} ns");
    if given >= 2.0 {
        assert!(
            free > free_wall + 1_000_000_000,
            "{free} ns of CPU in {free_wall} ns under no budget, where the host gave a program's eight threads {given:.2} CPUs"
        );
    } else {
        eprintln!(
            "SKIP: under no budget, {free} ns of CPU in {free_wall} ns: the host gave a program's eight threads {given:.2} CPUs, too few to show a hold of one"
        );
    }
    assert_eq!(shards(&["rm", "-f", "ga", "gb"]).status, Some(0));
    for mut run in runs {
        let _ = exit(&mut run);
    }
}

/// PM M370: a group's `cpu.max` held across its microVMs each second (D183, slice 3), as
/// M361 read a job's CPU rate hard cap: two microVMs under a group, four threads burning in
/// each for `M370_SECS` seconds (30), their VM processes' CPU time read from the host each
/// second, under a budget of one CPU's time each 100 ms period, of two, and of none; each
/// second's CPUs told as n, p50, p90, p99 and max (nearest rank), with the burn's mean.
#[test]
#[ignore = "a measurement: docs/research/measurements/cgroup-parent/cpu.sh"]
fn a_groups_cpu_held_each_second() {
    if !cfg!(any(target_os = "macos", windows)) {
        return;
    }
    let Some((home, image)) = home("containers-m370") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let secs: u64 = std::env::var("M370_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let group = home.join("groups").join("cpu");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cpu.max"), "max 100000\n").unwrap();
    let names = ["ma", "mb"];
    let runs: Vec<Child> = names
        .iter()
        .map(|name| {
            start(
                &home,
                &image,
                &["--name", name, "--cgroup-parent", "cpu"],
                &["sleep"],
            )
        })
        .collect();
    let pids: Vec<u32> = names
        .iter()
        .map(|name| {
            shards(&["inspect", "-f", "{{.State.Pid}}", name])
                .stdout
                .trim()
                .parse()
                .unwrap()
        })
        .collect();
    let cpu = || {
        pids.iter()
            .map(|&pid| shards_vmm::platform::process_usage(pid).unwrap().cpu_ns)
            .sum::<u64>()
    };
    let quantile = |sorted: &[f64], q: f64| {
        let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted[rank - 1]
    };
    let burn_ms = (secs * 1000 + 2000).to_string();
    for (label, budget) in [
        ("one CPU", "100000 100000"),
        ("two CPUs", "200000 100000"),
        ("no budget", "max 100000"),
    ] {
        std::fs::write(group.join("cpu.max"), format!("{budget}\n")).unwrap();
        // Heard by the daemon within a pressure window while the group has microVMs.
        std::thread::sleep(shards_vmm::platform::PRESSURE_WINDOW * 2);
        let (each, mean) = std::thread::scope(|s| {
            let burns: Vec<_> = names
                .iter()
                .map(|name| {
                    let burn_ms = &burn_ms;
                    s.spawn(move || shards(&["exec", name, "/bin/testguest", "burn", "4", burn_ms]))
                })
                .collect();
            // Both burning: a second in, then each second.
            std::thread::sleep(Duration::from_secs(1));
            let (first, t0) = (cpu(), Instant::now());
            let (mut last, mut at) = (first, t0);
            let mut each = Vec::new();
            for i in 1..=secs {
                std::thread::sleep((t0 + Duration::from_secs(i)).saturating_duration_since(Instant::now()));
                let (now, then) = (cpu(), Instant::now());
                each.push((now - last) as f64 / then.duration_since(at).as_nanos() as f64);
                (last, at) = (now, then);
            }
            let mean = (last - first) as f64 / at.duration_since(t0).as_nanos() as f64;
            for b in burns {
                let burned = b.join().unwrap();
                assert!(burned.stdout.ends_with("burned\n"), "{burned}");
            }
            (each, mean)
        });
        let mut sorted = each.clone();
        sorted.sort_by(f64::total_cmp);
        eprintln!(
            "M370 {label}: CPUs used each second: n {} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2}, mean {mean:.2}",
            sorted.len(),
            quantile(&sorted, 0.5),
            quantile(&sorted, 0.9),
            quantile(&sorted, 0.99),
            quantile(&sorted, 1.0)
        );
        let said: Vec<String> = each.iter().map(|c| format!("{c:.2}")).collect();
        eprintln!("M370 {label}: each second's CPUs: {}", said.join(" "));
    }
    assert_eq!(shards(&["rm", "-f", "ma", "mb"]).status, Some(0));
    for mut run in runs {
        let _ = exit(&mut run);
    }
}

/// A group shards keeps on Windows (D183, slice 3, PM M359): its microVMs' VM processes made
/// in the group's job, whose memory limit is the group's `memory.max`, so that what they
/// commit together stays under it: a run under a group of 1 GiB that allocates 2 GiB has
/// its guest's memory refused past it, its command never holding it all, where a run that
/// allocates as much under no group, and one that allocates within the group's room, run to
/// their ends. The macOS test below holds its group by give-backs from a host directory's
/// files, which Windows shares with the platform parity plan's step 6; the test after it,
/// by give-backs of what a joiner read of its image, on both.
#[test]
fn a_groups_job_holds_its_microvms_to_its_memory_max() {
    if !cfg!(windows) {
        return;
    }
    let Some((home, image)) = home("containers-group-job") else {
        return;
    };
    // Its budget written before its first microVM, as a user writes a cgroup's: the group's
    // directory, as its first run would make it, its files as the user left them.
    let group = home.join("groups").join("capped");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("memory.max"), "1G\n").unwrap();
    std::fs::write(group.join("memory.high"), "max\n").unwrap();
    // As much under no group: its memory grows as its workload needs it (D148).
    let free = run_in(&home, &image, &["--rm"], &["alloc-hold", "2048", "0"]);
    assert_eq!(
        (free.status, free.stdout.as_str()),
        (Some(0), "allocated 2048\n"),
        "{free}"
    );
    let within = run_in(
        &home,
        &image,
        &["--rm", "--cgroup-parent", "capped"],
        &["alloc-hold", "128", "0"],
    );
    assert_eq!(
        (within.status, within.stdout.as_str()),
        (Some(0), "allocated 128\n"),
        "{within}"
    );
    let past = run_in(
        &home,
        &image,
        &["--rm", "--cgroup-parent", "capped"],
        &["alloc-hold", "2048", "0"],
    );
    assert_ne!(past.status, Some(0), "{past}");
    assert!(!past.stdout.contains("allocated 2048"), "{past}");
}

/// A microVM under a group shards keeps (D183, slice 3), on macOS, whose host has no group
/// of processes a user's tools make and cap: made with the group's directory of the home,
/// its budget files `max`, the group's `memory.current` its VM processes' footprints; with
/// its `memory.high` written short of what it holds, told so and asked to give back what it
/// holds cold, its group within its budget again; and a microVM of the group that holds
/// past its `memory.max` with nothing to give back stopped, OOM-killed, as a cgroup's OOM
/// killer kills within it.
#[test]
fn a_microvm_under_a_group_is_held_to_its_budget() {
    if !cfg!(target_os = "macos") || cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("containers-group");
    let (kernel, init) = (kernel(), guest_init());
    // As the budget test's: its share's files come through FUSE into its guest's page
    // cache, which is what it holds cold.
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_SHARE_WINDOW_MIB", "0".as_ref()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let vol = TempDir::new("group-vol");
    let files: Vec<String> = (0..512).map(|i| format!("/vol/f{i}")).collect();
    for file in &files {
        std::fs::File::create(vol.join(file.trim_start_matches("/vol/")))
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
    }
    let mount = format!("{}:/vol", vol.display());
    let args = run_args(
        &image,
        &["-d", "--name", "held", "--cgroup-parent", "agents", "-v", &mount],
        &["sleep"],
    );
    let started = shards(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(started.status, Some(0), "{started}");
    let group = home.join("groups").join("agents");
    for file in ["memory.high", "memory.max"] {
        assert_eq!(
            std::fs::read_to_string(group.join(file)).unwrap(),
            "max\n",
            "{file}"
        );
    }
    let log = home.join("daemon.log");
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    let current = || -> u64 {
        std::fs::read_to_string(group.join("memory.current"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    };
    let until = |what: &str, check: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + TIMEOUT;
        while !check() {
            assert!(std::time::Instant::now() < deadline, "{what}:\n{}", said());
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mut read = vec!["exec", "held", "/bin/testguest", "read"];
    read.extend(files.iter().map(String::as_str));
    let read = shards(&read);
    assert_eq!(read.status, Some(0), "{read}");
    // Its guest holds what it read: past 512 MiB in its VM process's footprint.
    until("its group's memory.current never held what it read", &|| {
        current() > 512 << 20
    });
    let holding = current();
    let high = holding - (256 << 20);
    std::fs::write(group.join("memory.high"), format!("{high}\n")).unwrap();
    until("its group was not said short of its budget", &|| {
        said().contains("group agents: its microVMs hold") && said().contains("past its budget")
    });
    until("nothing given back said", &|| {
        said().contains("its microVMs gave back")
    });
    until("its group was not within its budget again", &|| {
        said().contains("within its budget of") && current() <= high
    });
    // Past its memory.max, which a microVM that holds 768 MiB its guest uses takes it past,
    // with nothing to give back.
    std::fs::write(group.join("memory.high"), "max\n").unwrap();
    std::fs::write(group.join("memory.max"), format!("{}\n", current() + (512 << 20))).unwrap();
    let args = run_args(
        &image,
        &["-d", "--name", "hog", "--cgroup-parent", "agents", "-m", "1g"],
        &["alloc-hold", "768", "600"],
    );
    let hog = shards(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(hog.status, Some(0), "{hog}");
    let waited = shards(&["wait", "hog"]);
    assert_eq!(waited.stdout.trim(), "137", "{waited}\n{}", said());
    let oom = shards(&["inspect", "-f", "{{.State.OOMKilled}}", "hog"]);
    assert_eq!(oom.stdout.trim(), "true", "{oom}");
    assert!(said().contains("past its memory.max"), "{}", said());
    // The other runs on: what it held it gave back, and held no more.
    let ps = shards(&["ps", "-q", "--filter", "name=held"]);
    assert_eq!(ps.stdout.lines().count(), 1, "{ps}");
    assert_eq!(shards(&["kill", "held"]).status, Some(0));
    let log = said();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("group agents") || l.contains("gave back") || l.contains("memory.max"))
        .collect();
    eprintln!(
        "its group held its microVMs to its budget, {holding} bytes held as it was set:\n{}",
        lines.join("\n")
    );
}

/// What a joiner reads of its image its microVM gives back to its group's budget (D119,
/// D188; D183, slice 3): a joiner's root is its image's EROFS over its microVM's join disk,
/// read through its guest's page cache, where its provider's root and every run's writable
/// layer are their host's files mapped into the guest (DAX, D190) and hold none of it. So a
/// microVM whose joiner read 512 MiB of its image holds them cold, on a host that shares
/// no directory into a microVM yet as on any other (Windows', with the platform parity
/// plan's step 6). With its group's `memory.high` written a quarter GiB short of what its VM
/// process holds, the microVM is told its room and asked to give back what it holds cold
/// (`ROOM`, `RECLAIM`, over its run's cgroup and its joiners' within it), gives back what
/// its joiner read, and its VM process holds that much less, its group within its budget
/// again.
#[test]
fn what_a_joiner_reads_of_its_image_is_given_back_at_its_groups_budget() {
    if !cfg!(any(target_os = "macos", windows)) {
        return;
    }
    let Some((home, image)) = home("containers-group-joiner") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let ctx = home.join("big");
    std::fs::create_dir(&ctx).unwrap();
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {image}\nCOPY big /big\n")).unwrap();
    noise(&ctx.join("big"), 512);
    let built = shards_in_within(
        &home,
        &["build", "-q", "-t", "big:1", ctx.to_str().unwrap()],
        Duration::from_secs(600),
    );
    assert_eq!(built.status, Some(0), "{built}");
    std::fs::remove_file(ctx.join("big")).unwrap();
    let provider = run_in(
        &home,
        &image,
        &["-d", "--name", "prov", "--cgroup-parent", "agents"],
        &["sleep"],
    );
    assert_eq!(provider.status, Some(0), "{provider}");
    let joiner = run_in(
        &home,
        "big:1",
        &["-d", "--name", "reader", "--network", "container:prov"],
        &["sleep"],
    );
    assert_eq!(joiner.status, Some(0), "{joiner}");
    let group = home.join("groups").join("agents");
    let log = home.join("daemon.log");
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    let current = || -> u64 {
        std::fs::read_to_string(group.join("memory.current"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    };
    let until = |what: &str, check: &dyn Fn() -> bool| {
        let deadline = Instant::now() + TIMEOUT;
        while !check() {
            assert!(
                Instant::now() < deadline,
                "{what}: its group's memory.current {}:\n{}",
                current(),
                said()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // What its VM process holds idle, as the daemon writes it each pressure window.
    until("its group's memory.current never written", &|| current() > 0);
    let idle = current();
    // What its guest has and holds of files, as its kernel counts them.
    let guest = || {
        shards(&[
            "exec",
            "prov",
            "/bin/testguest",
            "meminfo",
            "MemTotal",
            "MemFree",
            "Cached",
        ])
        .stdout
        .trim()
        .replace('\n', ", ")
    };
    // Its guest holds what its joiner read: past 512 MiB in its VM process's footprint. A
    // run is held at the memory it has plugged, its grower plugging more as it reaches it
    // (D148), and a read faster than the plugging is reclaimed in part by its guest's own
    // kernel: on WHP 131 to 378 MiB of the 512 stayed after one read (PM M391). The file is
    // read again into what it has plugged by then, a few times at most.
    let mut reads = 0;
    let after_read = loop {
        let read = shards(&["exec", "reader", "/bin/testguest", "read", "/big"]);
        assert!(read.stdout.starts_with("read /big 536870912 "), "{read}");
        reads += 1;
        // Its group's memory.current is written each pressure window.
        let written = Instant::now() + shards_vmm::platform::PRESSURE_WINDOW * 2;
        while current() <= 512 << 20 && Instant::now() < written {
            std::thread::sleep(Duration::from_millis(50));
        }
        if current() > 512 << 20 {
            break guest();
        }
        assert!(
            reads < 4,
            "its group's memory.current, {idle} idle, never held what its joiner read in {reads} reads ({}): {}:\n{}",
            guest(),
            current(),
            said()
        );
    };
    let holding = current();
    let high = holding - (256 << 20);
    std::fs::write(group.join("memory.high"), format!("{high}\n")).unwrap();
    until("its group was not said short of its budget", &|| {
        said().contains("group agents: its microVMs hold") && said().contains("past its budget")
    });
    // Its VM process holds the quarter GiB less, or more: its guest's own kernel takes back
    // as much of its files as it is short as it is told its room, before its give-back counts
    // what it reclaims, which may then be nothing (D188).
    until("its group was not within its budget again", &|| {
        said().contains("within its budget of") && current() <= high
    });
    let lines: Vec<String> = said()
        .lines()
        .filter(|l| l.contains("group agents"))
        .map(str::to_string)
        .collect();
    eprintln!(
        "a joiner's 512 MiB read under a group: {idle} bytes held idle, {holding} after {reads} reads ({after_read}), {} once it gave back ({}):\n{}",
        current(),
        guest(),
        lines.join("\n")
    );
    assert_eq!(shards(&["rm", "-f", "reader", "prov"]).status, Some(0));
}

/// The pipe a test tells its daemon its host is short of memory through
/// (`SHARDS_PRESSURE`, D188), each line a word: a FIFO on Unix, a named pipe on Windows,
/// there before the daemon opens it as it starts.
struct Told {
    path: std::ffi::OsString,
    #[cfg(windows)]
    server: std::fs::File,
}

impl Told {
    fn new(home: &Path) -> Told {
        #[cfg(unix)]
        {
            let path = home.join("pressure");
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: mkfifo(3) of a NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            Told {
                path: path.into_os_string(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
            use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
            let _ = home;
            let pipe = format!(r"\\.\pipe\shards-test-pressure-{}", std::process::id());
            let name: Vec<u16> = pipe.encode_utf16().chain(Some(0)).collect();
            // SAFETY: a NUL-terminated name and plain arguments.
            let server = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_WAIT,
                    1,
                    4096,
                    4096,
                    0,
                    std::ptr::null(),
                )
            };
            assert_ne!(
                server,
                INVALID_HANDLE_VALUE,
                "{pipe}: {}",
                std::io::Error::last_os_error()
            );
            Told {
                path: pipe.into(),
                // SAFETY: the pipe just made, ours alone.
                server: std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(server) }),
            }
        }
    }

    /// Its host's word that it is short of memory, once.
    fn say(&mut self) {
        use std::io::Write as _;
        #[cfg(unix)]
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        #[cfg(windows)]
        self.server.write_all(b"\n").unwrap();
    }
}

/// What a microVM holds cold it gives back at its host's word, on every host (D188): its
/// daemon told its host is short of memory through a pipe it opened as it started
/// (`SHARDS_PRESSURE`, as no unprivileged process may raise macOS's or Windows' pressure), a
/// microVM whose joiner read 512 MiB of its image, which its guest's page cache holds
/// (D119), gives back what it holds cold, its daemon says how much, and its VM process holds
/// that much less.
#[test]
fn what_a_microvm_holds_cold_is_given_back_at_its_hosts_word() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("containers-host-word");
    let mut told = Told::new(&home);
    let pipe = told.path.clone();
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_PRESSURE", pipe.as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    // Its daemon, started by the pull, hears its host's word from the pipe.
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let ctx = home.join("big");
    std::fs::create_dir(&ctx).unwrap();
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {image}\nCOPY big /big\n")).unwrap();
    noise(&ctx.join("big"), 512);
    let built = run_shards_env(
        &[],
        &["build", "-q", "-t", "big:1", ctx.to_str().unwrap()],
        &env,
        Duration::from_secs(600),
    );
    assert_eq!(built.status, Some(0), "{built}");
    std::fs::remove_file(ctx.join("big")).unwrap();
    let provider = shards(
        &run_args(&image, &["-d", "--name", "prov"], &["sleep"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert_eq!(provider.status, Some(0), "{provider}");
    let joiner = shards(
        &run_args(
            "big:1",
            &["-d", "--name", "reader", "--network", "container:prov"],
            &["sleep"],
        )
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>(),
    );
    assert_eq!(joiner.status, Some(0), "{joiner}");
    let pid: u32 = shards(&["inspect", "-f", "{{.State.Pid}}", "prov"])
        .stdout
        .trim()
        .parse()
        .unwrap();
    let held = || shards_vmm::platform::process_usage(pid).unwrap().footprint;
    let log = home.join("daemon.log");
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    let idle = held();
    // Its guest holds what its joiner read, a quarter of it or more, read again into what it
    // has plugged where a read outran its plugging (D148, PM M391), a few times at most.
    let mut reads = 0;
    while held() <= idle + (128 << 20) {
        assert!(
            reads < 4,
            "{idle} bytes held idle, {} after {reads} reads",
            held()
        );
        let read = shards(&["exec", "reader", "/bin/testguest", "read", "/big"]);
        assert!(read.stdout.starts_with("read /big 536870912 "), "{read}");
        reads += 1;
    }
    let before = held();
    let word = Instant::now();
    told.say();
    let deadline = word + TIMEOUT;
    let gave = loop {
        let given = said()
            .lines()
            .filter_map(|l| l.split_once("the host is short of memory: its microVMs gave back "))
            .find_map(|(_, rest)| rest.split_whitespace().next()?.parse::<u64>().ok());
        if let Some(gave) = given {
            break gave;
        }
        assert!(Instant::now() < deadline, "nothing given back said:\n{}", said());
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(gave > 0, "nothing given back:\n{}", said());
    // Its VM process holds that much less, or half of it, once its guest has given it back:
    // what it reported free, and what its device unplugs as it was asked to, after the
    // give-back's answer (D148, D151).
    let deadline = Instant::now() + TIMEOUT;
    while before.saturating_sub(held()) < (gave << 20) / 2 {
        assert!(
            Instant::now() < deadline,
            "its VM process held {before} bytes, then {}, having given back {gave} MiB:\n{}",
            held(),
            said()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "a joiner's 512 MiB read: {idle} bytes held idle, {before} after {reads} reads, {} {:?} after its host's word, {gave} MiB given back",
        held(),
        word.elapsed()
    );
    assert_eq!(shards(&["rm", "-f", "reader", "prov"]).status, Some(0));
}

/// A microVM held to a budget reads a file through its volume's DAX window to its end
/// (D188, D189): under a slice of the user's manager whose `MemoryHigh` is past the microVM
/// idle and short of the file, the file is mapped through the window, its pages its host's
/// page cache charged to the slice, which the host reclaims itself as the read goes on (no
/// guest's page cache to give back), and the read ends, the budget met on the way (the
/// slice's `memory.events`, `high`). Its daemon is given a window, which a host that is
/// itself a virtual machine on an AMD CPU gives none by default (PM M308), and the read up
/// to 300 s: such a host reads through one at 7 to 25 MiB/s (PM M308). Linux alone, as the
/// budget test before it.
// Unix's: a slice of the user's manager, at the user's ID (getuid). A volume's DAX window on
// Windows comes with the platform parity plan's step 6.
#[cfg(unix)]
#[test]
fn a_microvm_held_to_a_budget_reads_a_large_file_through_its_window_to_its_end() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Some((home, image)) = home("containers-budget-dax") else {
        return;
    };
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
    // The daemon the pull started stopped, and one given a 4 GiB window started for the run.
    assert_eq!(shards_in(&home, &["stop", "daemon"]).status, Some(0));
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_SHARE_WINDOW_MIB", "4096".as_ref()),
    ];
    let shards = |args: &[&str], within: Duration| run_shards_env(&[], args, &env, within);
    let vol = TempDir::new("budget-dax-vol");
    std::fs::File::create(vol.join("big"))
        .unwrap()
        .set_len(512 << 20)
        .unwrap();
    let slice = format!("shardsbudgetdax{}.slice", std::process::id());
    /// The slice's budget taken off and the slice stopped, however the test ends.
    struct Budget<'a>(&'a str);
    impl Drop for Budget<'_> {
        fn drop(&mut self) {
            for verb in ["revert", "stop"] {
                let _ = Command::new("systemctl").args(["--user", verb, self.0]).status();
            }
        }
    }
    let _budget = Budget(&slice);
    let mount = format!("{}:/vol", vol.display());
    let args = run_args(
        &image,
        &["-d", "--name", "budget", "--cgroup-parent", &slice, "-v", &mount],
        &["sleep"],
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let started = shards(&args, TIMEOUT);
    assert_eq!(started.status, Some(0), "{started}");
    let set = Command::new("systemctl")
        .args(["--user", "set-property", "--runtime", &slice])
        .args(["MemoryHigh=384M", "MemorySwapMax=0"])
        .output()
        .unwrap();
    assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));
    let dax = shards(
        &["exec", "budget", "/bin/testguest", "fs", "dax:/vol/big"],
        TIMEOUT,
    );
    assert_eq!(dax.stdout, "dax /vol/big yes\n", "{dax}");
    let read = shards(
        &["exec", "budget", "/bin/testguest", "read", "/vol/big"],
        Duration::from_secs(300),
    );
    assert_eq!(read.status, Some(0), "{read}");
    assert!(read.stdout.starts_with("read /vol/big 536870912 "), "{read}");
    // SAFETY: getuid(2) takes nothing and cannot fail.
    let uid = unsafe { libc::getuid() };
    let events = std::fs::read_to_string(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{slice}/memory.events"
    ))
    .unwrap();
    let high: u64 = events
        .lines()
        .find_map(|l| l.strip_prefix("high "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    assert!(high > 0, "the slice's budget was never met: {events}");
    assert_eq!(shards(&["kill", "budget"], TIMEOUT).status, Some(0));
}

#[test]
fn pause_freezes_a_microvm_until_unpause_or_stop() {
    let Some((home, image)) = home("containers-pause") else {
        return;
    };
    // Under --init, so that the spinner hears stop's SIGTERM, which it does not handle.
    let mut spinner = start(&home, &image, &["--name", "frozen", "--init"], &["spin"]);
    let shards = |args: &[&str]| shards_in(&home, args);
    // Its share of a CPU, as `stats` measures it over a second.
    let cpu = || -> f64 {
        let stats = shards(&["watch", "vms", "--no-stream", "frozen"]);
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
    assert!(again.stderr.ends_with("is paused already\n"), "{again}");
    assert!(cpu() < 5.0, "a frozen microVM spun");
    // As the host reads its VM process, where `stats` shows a frozen microVM's last
    // sample: its spinning guest held, it takes next to no CPU time.
    let pid: u32 = shards(&["inspect", "-f", "{{.State.Pid}}", "frozen"])
        .stdout
        .trim()
        .parse()
        .unwrap();
    let before = shards_vmm::platform::process_usage(pid).unwrap().cpu_ns;
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    let used = shards_vmm::platform::process_usage(pid).unwrap().cpu_ns - before;
    let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
    assert!(
        used < wall / 20,
        "a frozen microVM's VM process used {used} ns of CPU in {wall} ns"
    );
    let ps = shards(&["ps"]);
    assert!(ps.stdout.contains(" (Paused) "), "{ps}");
    let inspected = shards(&["container", "inspect", "frozen"]);
    assert!(inspected.stdout.contains(r#""Status": "paused""#), "{inspected}");
    let rm = shards(&["rm", "frozen"]);
    assert!(rm.stderr.ends_with("it is paused: unpause it first\n"), "{rm}");
    // Not started, nor said to be, as dockerd starts no paused container (D27).
    let start = shards(&["start", "frozen"]);
    assert_eq!(
        (start.status, start.stdout.as_str(), start.stderr.as_str()),
        (
            Some(1),
            "",
            "shards: cannot start a paused container, try unpause instead\nfailed to start containers: frozen\n"
        ),
        "{start}"
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
    // A stop lets a frozen microVM go on to hear its signal, as dockerd's does (D27).
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
fn top_lists_a_microvms_processes() {
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
        (Some(1), "shards: specifying \"user=PID\" is not allowed\n"),
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
fn diff_shows_what_a_microvm_changed() {
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
    // A parent of a change is changed; /etc/hosts is init's, as Docker's init layer's (D37).
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

/// What happened is kept past the daemon that heard it (D187): the next, started by the
/// next command as an idle daemon's exit leaves it to be, reads it back for `--since`,
/// where dockerd's last 256 are gone with it.
#[test]
fn events_outlive_their_daemon() {
    let Some((home, image)) = home("containers-events-kept") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let ran = run_in(&home, &image, &["--rm", "--name", "kept"], &["exit", "3"]);
    assert_eq!(ran.status, Some(3), "{ran}");
    assert_eq!(shards(&["daemon", "stop"]).status, Some(0));
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
        "container=kept",
    ]);
    assert_eq!(past.status, Some(0), "{past}");
    let actions: Vec<&str> = past.stdout.lines().filter_map(|l| l.split(' ').nth(2)).collect();
    // Its life, in order, as the daemon that ran it heard it.
    let order = ["create", "start", "die", "destroy"];
    let mut at = 0;
    for action in &actions {
        if order.get(at) == Some(action) {
            at += 1;
        }
    }
    assert_eq!(at, order.len(), "{past}");
    assert!(past.stdout.contains("exitCode=3"), "{past}");
    // Kept in the home, an event a line, as `--format json` prints it.
    let journal: String = std::fs::read_dir(home.join("events"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert!(
        journal
            .lines()
            .any(|l| l.starts_with("{\"Type\":\"container\",\"Action\":\"die\"")),
        "{journal}"
    );
}

#[test]
fn events_tell_a_microvms_life() {
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
    // Under --init, so that stop's SIGTERM ends the command, which does not handle it: as
    // PID 1 it would not hear it, and stop would end it with SIGKILL after ten seconds,
    // a second kill (`a_command_is_pid_1_as_a_containers_is`).
    let mut sleeper = start(&home, &image, &["--name", "lived", "--init"], &["sleep"]);
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
fn cp_copies_files_into_and_out_of_a_microvm() {
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
    // In Docker's words (D37).
    let missing = shards(&["cp", "copier:/nope", home.join("x").to_str().unwrap()]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (
            Some(1),
            "shards: Could not find the file /nope in container copier\n"
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
fn a_stopped_microvms_files_are_read_and_written() {
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
    // Its changes, read in a VM over its files, as dockerd reads a stopped container's (D37).
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
    // top, as dockerd's, reads only a running one (D123).
    let top = shards(&["top", "resting"]);
    assert_eq!(top.status, Some(1), "{top}");
}

/// A container started again and refused before it starts is left as it was: refused
/// in `docker start`'s words, the client naming it after, or `docker restart`'s, and
/// every command reaches it at once after, another start among them (D37).
#[test]
fn a_refused_start_leaves_its_container_as_it_was() {
    let Some((home, image)) = home("containers-start-refused") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let ok = |args: &[&str]| {
        let r = shards(args);
        assert_eq!(r.status, Some(0), "{args:?}: {r}");
    };
    ok(&["network", "create", "--subnet", "10.77.0.0/24", "moved"]);
    ok(&[
        "create",
        "--pull",
        "never",
        "--name",
        "left",
        "--network",
        "moved",
        "--ip",
        "10.77.0.5",
        &image,
        "exit",
        "0",
    ]);
    // Its network made again elsewhere: no subnet of it holds the address it was given.
    ok(&["network", "rm", "moved"]);
    ok(&["network", "create", "--subnet", "10.78.0.0/24", "moved"]);
    let started = shards(&["start", "left"]);
    assert_eq!(started.status, Some(1), "{started}");
    let why = started
        .stderr
        .strip_prefix("shards: ")
        .and_then(|s| s.strip_suffix("\nfailed to start containers: left\n"))
        .unwrap_or_else(|| panic!("{started}"));
    assert!(why.contains("10.77.0.5"), "{started}");
    // Reached at once: its files, another start, refused alike, a restart, in its words.
    let diff = shards(&["diff", "left"]);
    assert_eq!((diff.status, diff.stdout.as_str()), (Some(0), ""), "{diff}");
    let again = shards(&["start", "left"]);
    assert_eq!(
        (again.status, again.stderr.as_str()),
        (Some(1), started.stderr.as_str()),
        "{again}"
    );
    let restarted = shards(&["restart", "left"]);
    assert_eq!(
        (restarted.status, restarted.stderr),
        (Some(1), format!("shards: cannot restart left: {why}\n")),
    );
    let state = shards(&["inspect", "-f", "{{.State.Status}}", "left"]);
    assert_eq!(state.stdout, "created\n", "{state}");
    ok(&["rm", "left"]);
}

/// An endpoint's sysctls are its interface's, as dockerd sets them (PM M171): eth0's, after
/// the container's own; two in one option, in either case; a key the kernel has no file
/// for, or a value it refuses, failing the start in libnetwork's words; none on `none`;
/// and an interface's sysctl given as the container's own refused as it is made.
#[test]
fn an_endpoints_sysctls_are_its_interfaces() {
    let Some((home, image)) = home("containers-endpoint-sysctls") else {
        return;
    };
    let ep = "com.docker.network.endpoint.sysctls";
    let conf = "/proc/sys/net/ipv4/conf";
    let run = |network: String, more: &[&str], command: &[&str]| {
        let mut options = vec!["--rm", "--network", network.as_str()];
        options.extend(more);
        run_in(&home, &image, &options, command)
    };
    let read = |network: String, more: &[&str], files: &[&str]| {
        let mut command = vec!["fs".to_string()];
        command.extend(files.iter().map(|f| format!("print:{conf}/{f}")));
        let command: Vec<&str> = command.iter().map(String::as_str).collect();
        let r = run(network, more, &command);
        assert_eq!(r.status, Some(0), "{r}");
        r.stdout
    };
    assert_eq!(
        read(
            format!("name=bridge,driver-opt={ep}=net.ipv4.conf.IFNAME.log_martians=1"),
            &[],
            &["eth0/log_martians"]
        ),
        "1\n"
    );
    assert_eq!(
        read(
            format!(
                "name=bridge,\"driver-opt={ep}=net.ipv4.conf.ifname.log_martians=1,net.ipv4.conf.IFNAME.accept_redirects=0\""
            ),
            &[],
            &["eth0/log_martians", "eth0/accept_redirects"]
        ),
        "1\n0\n"
    );
    // The endpoint's after the container's own: all.forwarding sets every interface's.
    assert_eq!(
        read(
            format!("name=bridge,driver-opt={ep}=net.ipv4.conf.IFNAME.forwarding=0"),
            &["--sysctl", "net.ipv4.conf.all.forwarding=1"],
            &["all/forwarding", "eth0/forwarding"]
        ),
        "1\n0\n"
    );
    let failed = |network: String, said: &str| {
        let r = run(network, &[], &["exit", "0"]);
        assert!(r.status == Some(125) && r.stderr.contains(said), "{r}");
    };
    failed(
        format!("name=bridge,driver-opt={ep}=net.ipv4.conf.IFNAME.nonexistent=1"),
        "failed to set up container networking: failed to add interface eth0 to sandbox: /proc/sys/net/ipv4/conf/eth0/nonexistent is not a sysctl file",
    );
    failed(
        format!("name=bridge,driver-opt={ep}=net.ipv4.conf.IFNAME.log_martians=abc"),
        "failed to set up container networking: failed to add interface eth0 to sandbox: unable to write to '/proc/sys/net/ipv4/conf/eth0/log_martians': write /proc/sys/net/ipv4/conf/eth0/log_martians: invalid argument",
    );
    // `none` has no interface: dockerd sets none, and the run goes on.
    let none = run(
        format!("name=none,driver-opt={ep}=net.ipv4.conf.IFNAME.log_martians=1"),
        &[],
        &["exit", "0"],
    );
    assert_eq!(none.status, Some(0), "{none}");
    // An interface's sysctl as the container's own: refused as it is made.
    let said = "interface specific sysctl setting \"net.ipv4.conf.eth0.log_martians\" must be supplied using driver option 'com.docker.network.endpoint.sysctls'";
    let own = run_in(
        &home,
        &image,
        &["--rm", "--sysctl", "net.ipv4.conf.eth0.log_martians=1"],
        &["exit", "0"],
    );
    assert!(own.status == Some(125) && own.stderr.contains(said), "{own}");
    let made = shards_in(
        &home,
        &["create", "--sysctl", "net.ipv4.conf.eth0.log_martians=1", &image],
    );
    assert_eq!(
        (made.status, made.stderr.as_str()),
        (Some(1), format!("shards: {said}\n").as_str()),
        "{made}"
    );
}

/// A run's MAC is its own, as dockerd gives it (D46, PM M173): one given, either way and in
/// any form Go's ParseMAC reads, is eth0's, an EUI-64's first six octets; else one is made
/// for each run, local unicast, so that no two runs of one template share theirs. inspect
/// shows it while it runs and, given, before its first start, and none once it has run;
/// one no interface can have fails the start in libnetwork's words; on `none` it is taken,
/// and no interface has it. A published port reaches a run past its change, and two given
/// one MAC on a network reach one another, as on Docker's bridge they would not.
/// A network's egress is a grant of its destinations alone (D192, its `shards.egress`;
/// networks are default deny): its member's own command reaches each destination it names,
/// by name, at the addresses that name resolved to through the member's network process,
/// and nothing else: not another port of the same host, not a name the destinations do not
/// name, not the host's address unasked; a run of the default bridge reaches none of it.
/// The option is refused where it names no destination, or where nothing could reach one.
#[test]
fn a_networks_egress_is_a_grant_of_its_destinations_alone() {
    use std::io::Write as _;
    if cannot_run_vms() {
        return;
    }
    // The host's own address on its network, which the guest's flows reach it at.
    let host = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    let Ok(std::net::SocketAddr::V4(host)) = host else {
        eprintln!("SKIP: this host has no IPv4 route to give a guest an address of it");
        return;
    };
    let address = *host.ip();
    let serve = |hello: &'static str| {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in listener.incoming().flatten() {
                let mut c = c;
                let _ = c.write_all(hello.as_bytes());
            }
        });
        port
    };
    let (granted, other) = (serve("granted"), serve("other"));
    // The host's resolver, as the daemon is told to ask (SHARDS_DNS): api.example and
    // also.example are this host, so that a name refused is shards' refusal, not the
    // resolver's.
    let resolver = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let resolver_at = format!("{address}:{}", resolver.local_addr().unwrap().port());
    std::thread::spawn(move || {
        let mut q = [0u8; 512];
        while let Ok((n, from)) = resolver.recv_from(&mut q) {
            let q = &q[..n];
            let mut at = 12;
            let mut name = Vec::new();
            while at < q.len() && q[at] != 0 {
                let len = usize::from(q[at]);
                name.push(String::from_utf8_lossy(&q[at + 1..at + 1 + len]).to_lowercase());
                at += 1 + len;
            }
            let a = q.get(at + 1..at + 3) == Some(&[0, 1]);
            let count = u8::from(a && matches!(name.join(".").as_str(), "api.example" | "also.example"));
            let mut r = q[..2].to_vec();
            r.extend_from_slice(&[0x81, 0x80, 0, 1, 0, count, 0, 0, 0, 0]);
            r.extend_from_slice(&q[12..at + 5]);
            if count == 1 {
                r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                r.extend_from_slice(&address.octets());
            }
            let _ = resolver.send_to(&r, from);
        }
    });
    let (image, _) = served();
    let home = TempDir::new("containers-egress");
    let (kernel, init) = (kernel(), guest_init());
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_DNS", OsStr::new(&resolver_at)),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    // Refused as the network is made: a destination of no port, none at all, a pattern,
    // and an internal network's, whose members reach nothing past their microVMs.
    for (opt, internal, why) in [
        (
            "shards.egress=api.example",
            false,
            "--opt shards.egress: api.example: a destination is HOST:PORT",
        ),
        (
            "shards.egress=",
            false,
            "--opt shards.egress: an empty destination, where each is HOST:PORT",
        ),
        (
            "shards.egress=*.example:443",
            false,
            "--opt shards.egress: *.example:443: *.example is neither a host's name nor an IPv4 address",
        ),
        (
            "shards.egress=api.example:443",
            true,
            "--opt shards.egress: an internal network's members reach nothing past their microVMs",
        ),
    ] {
        let mut args = vec!["network", "create", "--opt", opt];
        if internal {
            args.push("--internal");
        }
        args.push("refused");
        let made = shards(&args);
        assert_ne!(made.status, Some(0), "{opt}: {made}");
        assert!(made.stderr.contains(why), "{opt}: {made}");
    }
    let option = format!("shards.egress=api.example:{granted}");
    let made = shards(&["network", "create", "--opt", &option, "out"]);
    assert_eq!(made.status, Some(0), "{made}");
    let run = |network: &str, command: &[&str]| {
        let mut args = vec!["run", "--rm", "--network", network, image.as_str()];
        args.extend(command);
        shards(&args)
    };
    // Its destination, by its name.
    let reached = run("out", &["tcp", &format!("api.example:{granted}")]);
    assert_eq!(reached.stdout, "tcp 7 granted\n", "{reached}");
    // Another port of the same host, whose name it may ask.
    let refused = run("out", &["tcp", &format!("api.example:{other}")]);
    assert!(refused.stdout.starts_with("tcp error"), "{refused}");
    // A name its destinations do not name, though the host's resolver has it.
    let unnamed = run("out", &["resolve", "also.example"]);
    assert!(unnamed.stdout.starts_with("resolve error"), "{unnamed}");
    // The destination's address, in a microVM that never asked for its name.
    let unasked = run("out", &["tcp", &format!("{address}:{granted}")]);
    assert!(unasked.stdout.starts_with("tcp error"), "{unasked}");
    // A run of the default bridge, whose network grants nothing past its microVM.
    let bridged = run("bridge", &["tcp", &format!("{address}:{granted}")]);
    assert!(bridged.stdout.starts_with("tcp error"), "{bridged}");
}

#[test]
fn a_runs_mac_is_its_own() {
    use std::io::{Read as _, Write as _};
    let Some((home, image)) = home("containers-mac") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let address = ["fs", "print:/sys/class/net/eth0/address"];
    let eth0 = |options: &[&str]| {
        let mut o = vec!["--rm"];
        o.extend(options);
        let r = run_in(&home, &image, &o, &address);
        assert_eq!(r.status, Some(0), "{options:?}: {r}");
        r.stdout
    };
    for (options, want) in [
        (&["--mac-address", "02:42:AC:11:00:99"][..], "02:42:ac:11:00:99\n"),
        (
            &["--network", "name=bridge,mac-address=02-42-ac-11-00-98"],
            "02:42:ac:11:00:98\n",
        ),
        (&["--mac-address", " 0242.ac11.0097 "], "02:42:ac:11:00:97\n"),
        (
            &["--mac-address", "02:42:ac:11:00:96:00:01"],
            "02:42:ac:11:00:96\n",
        ),
    ] {
        assert_eq!(eth0(options), want, "{options:?}");
    }
    // Made: each run's own, locally administered and unicast (IEEE 802-2014 8.2).
    let made = [eth0(&[]), eth0(&[])];
    assert_ne!(made[0], made[1]);
    for m in &made {
        let first = u8::from_str_radix(m.get(..2).unwrap(), 16).unwrap();
        assert_eq!(first & 0b11, 0b10, "{m}");
    }
    // Refused by the CLI, as docker/cli refuses them (D46).
    for (options, said) in [
        (
            &["--mac-address", "02:42:zz:11:00:99"][..],
            "02:42:zz:11:00:99 is not a valid mac address",
        ),
        (
            &["--network", "name=bridge,mac-address=bad"],
            "bad is not a valid mac address",
        ),
        (
            &[
                "--mac-address",
                "02:42:ac:11:00:02",
                "--network",
                "name=bridge,mac-address=02:42:ac:11:00:03",
            ],
            "conflicting options: cannot specify both --mac-address and per-network MAC address",
        ),
    ] {
        let r = run_in(&home, &image, options, &["exit", "0"]);
        assert!(
            r.status == Some(125) && r.stderr.contains(said),
            "{options:?}: {r}"
        );
    }
    // One no interface can have fails the start, in libnetwork's words.
    for bad in ["01:00:5e:00:00:01", "00:00:00:00:00:00"] {
        let r = run_in(&home, &image, &["--rm", "--mac-address", bad], &["exit", "0"]);
        let said = format!(
            "failed to set up container networking: failed to add interface eth0 to sandbox: error setting interface \"eth0\" MAC to \"{bad}\": cannot assign requested address"
        );
        assert!(r.status == Some(125) && r.stderr.contains(&said), "{bad}: {r}");
    }
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "none", "--mac-address", "02:42:ac:11:00:95"],
        &address,
    );
    assert!(
        r.status == Some(1) && r.stderr.contains("No such file or directory"),
        "{r}"
    );
    // What inspect shows.
    let nets = "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} [{{$v.MacAddress}}]{{end}}";
    let shown = |name: &str| shards(&["inspect", "-f", nets, name]).stdout;
    for (name, options, want) in [
        (
            "given",
            &["--mac-address", "02:42:AC:11:00:94:00:01"][..],
            "bridge [02:42:ac:11:00:94]\n",
        ),
        ("unset", &[], "bridge []\n"),
        (
            "offline",
            &["--network", "none", "--mac-address", "02:42:ac:11:00:94"],
            "none []\n",
        ),
    ] {
        let mut args = vec!["create", "--pull", "never", "--name", name];
        args.extend(options);
        args.extend([image.as_str(), "exit", "0"]);
        let created = shards(&args);
        assert_eq!(created.status, Some(0), "{created}");
        assert_eq!(shown(name), want, "{name}");
    }
    let ran = shards(&["start", "-a", "given"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    assert_eq!(shown("given"), "bridge []\n", "once it has run");
    // Running: its own, given or made, as eth0 has it; and a published port reaches it.
    let mut own = start(&home, &image, &["--name", "own"], &["sleep"]);
    let has = shards(&[
        "exec",
        "own",
        "/bin/testguest",
        "fs",
        "print:/sys/class/net/eth0/address",
    ]);
    assert_eq!(shown("own"), format!("bridge [{}]\n", has.stdout.trim()), "{has}");
    // Given on another's network: taken, and the joiner's interface is its provider's.
    let joined = run_in(
        &home,
        &image,
        &[
            "--rm",
            "--network",
            "container:own",
            "--mac-address",
            "02:42:ac:11:00:91",
        ],
        &address,
    );
    assert_eq!(
        (joined.status, joined.stdout.as_str()),
        (Some(0), has.stdout.as_str()),
        "{joined}"
    );
    assert_eq!(shards(&["rm", "-f", "own"]).status, Some(0));
    let _ = exit(&mut own);
    let mut served = start(
        &home,
        &image,
        &[
            "--name",
            "served",
            "--mac-address",
            "02:42:ac:11:00:93",
            "-p",
            "7000",
        ],
        &["serve", "7000", "1"],
    );
    assert_eq!(shown("served"), "bridge [02:42:ac:11:00:93]\n");
    let listed = shards(&["port", "served", "7000"]);
    let port: u16 = listed
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("0.0.0.0:"))
        .unwrap()
        .parse()
        .unwrap();
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(TIMEOUT)).unwrap();
    c.write_all(b"hello").unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = String::new();
    c.read_to_string(&mut got).unwrap();
    assert_eq!(got, format!("from {}\nhello", bridge().gateway()));
    assert_eq!(exit(&mut served), Some(0));
    // Two of one MAC on a network: each reaches the other through its own network process,
    // which frames to it alone; network inspect shows each its MAC.
    let made = shards(&["network", "create", "maclan"]);
    assert_eq!(made.status, Some(0), "{made}");
    let same = ["--network", "name=maclan,mac-address=02:42:ac:11:00:92"];
    let mut srv = start(
        &home,
        &image,
        &[&["--name", "twin"][..], &same[..]].concat(),
        &["serve", "7000", "1"],
    );
    assert_eq!(shown("twin"), "maclan [02:42:ac:11:00:92]\n");
    let members = shards(&[
        "network",
        "inspect",
        "-f",
        "{{range .Containers}}{{.Name}} {{.MacAddress}}{{end}}",
        "maclan",
    ]);
    assert_eq!(members.stdout, "twin 02:42:ac:11:00:92\n", "{members}");
    let asked = run_in(
        &home,
        &image,
        &[&["--rm"][..], &same[..]].concat(),
        &["ask", "twin:7000"],
    );
    assert!(
        asked.status == Some(0) && asked.stdout.starts_with("ask from "),
        "{asked}"
    );
    assert_eq!(exit(&mut srv), Some(0));
}

/// A run's link-local addresses, as dockerd gives them (PM M175): each on eth0 with its
/// family's prefix, given either way, an IPv4-mapped one as IPv4, a malformed one dropped
/// as docker/cli drops it (D46); dockerd's refusals and failures in its words; none on `none`,
/// and none of a joiner's own; inspect's LinkLocalIPs while created, running and exited.
/// On a network, two given them reach each other at them and one without reaches neither;
/// on one with IPv6, a member reaches another's at the address its kernel makes of its
/// MAC, as on dockerd's bridge.
#[test]
fn a_runs_link_local_addresses_are_its_own() {
    let Some((home, image)) = home("containers-link-local") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let eth0 = |options: &[&str]| -> Vec<String> {
        let mut o = vec!["--rm"];
        o.extend(options);
        let r = run_in(&home, &image, &o, &["addrs"]);
        assert_eq!(r.status, Some(0), "{options:?}: {r}");
        r.stdout
            .lines()
            .filter_map(|l| l.strip_prefix("eth0 "))
            .map(String::from)
            .collect()
    };
    for (options, want) in [
        (&["--link-local-ip", "169.254.1.1"][..], &["169.254.1.1/16"][..]),
        (
            &[
                "--network",
                "name=bridge,link-local-ip=169.254.1.2,link-local-ip=169.254.7.7",
            ],
            &["169.254.1.2/16", "169.254.7.7/16"],
        ),
        (&["--link-local-ip", "::ffff:169.254.10.1"], &["169.254.10.1/16"]),
    ] {
        let got = eth0(options);
        for w in want {
            assert!(got.iter().any(|a| a == w), "{options:?}: {got:?}");
        }
    }
    let got = eth0(&["--link-local-ip", "nope"]);
    assert!(!got.iter().any(|a| a.starts_with("169.254.")), "{got:?}");
    let failed = "failed to set up container networking:";
    for (options, status, said) in [
        (
            &["--link-local-ip", "10.9.8.7"][..],
            125,
            "--link-local-ip 10.9.8.7: not a link-local address (169.254.0.0/16, fe80::/10)".to_string(),
        ),
        (
            &["--network", "none", "--link-local-ip", "10.9.8.7"],
            125,
            "--link-local-ip 10.9.8.7: not a link-local address (169.254.0.0/16, fe80::/10)".to_string(),
        ),
        (
            &["--link-local-ip", "169.254.12.1", "--link-local-ip", "10.9.8.6"],
            125,
            "--link-local-ip 10.9.8.6: not a link-local address (169.254.0.0/16, fe80::/10)".to_string(),
        ),
        (
            &["--link-local-ip", "fe80::1234"],
            126,
            format!(
                "{failed} failed to add interface eth0 to sandbox: error setting interface \"eth0\" link local IPs to [fe80::1234/64]: permission denied"
            ),
        ),
        (
            &["--link-local-ip", "169.254.8.1", "--link-local-ip", "169.254.8.1"],
            125,
            format!(
                "{failed} failed to add interface eth0 to sandbox: error setting interface \"eth0\" link local IPs to [169.254.8.1/16 169.254.8.1/16]: file exists"
            ),
        ),
        (
            &["--link-local-ip", "0.0.0.0"],
            125,
            "--network bridge: --link-local-ip 0.0.0.0: not an address a run can have".to_string(),
        ),
        (
            &[
                "--link-local-ip",
                "169.254.9.1",
                "--network",
                "name=bridge,link-local-ip=169.254.9.2",
            ],
            125,
            "conflicting options: cannot specify both --link-local-ip and per-network link-local IP addresses".to_string(),
        ),
    ] {
        let r = run_in(&home, &image, &[&["--rm"][..], options].concat(), &["exit", "0"]);
        assert!(r.status == Some(status) && r.stderr.contains(&said), "{options:?}: {r}");
    }
    // `none`: taken, and no interface but lo has it.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "none", "--link-local-ip", "169.254.1.3"],
        &["addrs"],
    );
    assert!(r.status == Some(0) && !r.stdout.contains("169.254."), "{r}");
    // inspect, as dockerd keeps it from the request (D123): created, running and exited alike.
    let ll = "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{json $v.IPAMConfig}}{{end}}";
    let shown = |name: &str| shards(&["inspect", "-f", ll, name]).stdout;
    for (name, options, want) in [
        (
            "ll-made",
            &[
                "--link-local-ip",
                "169.254.5.1",
                "--link-local-ip",
                "::ffff:169.254.5.2",
            ][..],
            "bridge {\"LinkLocalIPs\":[\"169.254.5.1\",\"169.254.5.2\"]}\n",
        ),
        (
            "ll-zoned",
            &["--link-local-ip", "fe80::1%eth0"],
            "bridge {\"LinkLocalIPs\":[\"fe80::1%eth0\"]}\n",
        ),
        (
            "ll-none",
            &["--network", "none", "--link-local-ip", "169.254.14.1"],
            "none {\"LinkLocalIPs\":[\"169.254.14.1\"]}\n",
        ),
        ("ll-unset", &[], "bridge null\n"),
    ] {
        let mut args = vec!["create", "--pull", "never", "--name", name];
        args.extend(options);
        args.extend([image.as_str(), "exit", "0"]);
        let created = shards(&args);
        assert_eq!(created.status, Some(0), "{created}");
        assert_eq!(shown(name), want, "{name}");
    }
    let ran = shards(&["start", "-a", "ll-made"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    assert_eq!(
        shown("ll-made"),
        "bridge {\"LinkLocalIPs\":[\"169.254.5.1\",\"169.254.5.2\"]}\n"
    );
    // A joiner: taken, and nothing of its own, its interface its provider's.
    let mut prov = start(
        &home,
        &image,
        &["--name", "ll-prov", "--link-local-ip", "169.254.6.2"],
        &["sleep"],
    );
    assert_eq!(
        shown("ll-prov"),
        "bridge {\"LinkLocalIPs\":[\"169.254.6.2\"]}\n",
        "while it runs"
    );
    let joined = run_in(
        &home,
        &image,
        &[
            "--rm",
            "--network",
            "container:ll-prov",
            "--link-local-ip",
            "169.254.6.1",
        ],
        &["addrs"],
    );
    assert!(
        joined.status == Some(0)
            && !joined.stdout.contains("169.254.6.1")
            && joined.stdout.contains("eth0 169.254.6.2/16"),
        "{joined}"
    );
    assert_eq!(shards(&["rm", "-f", "ll-prov"]).status, Some(0));
    let _ = exit(&mut prov);
    // On a network: two given them reach each other at them; one without, neither.
    let made = shards(&["network", "create", "llnet"]);
    assert_eq!(made.status, Some(0), "{made}");
    let mut srv = start(
        &home,
        &image,
        &[
            "--name",
            "ll-srv",
            "--network",
            "name=llnet,link-local-ip=169.254.2.10",
        ],
        &["serve", "7000", "1"],
    );
    let asked = run_in(
        &home,
        &image,
        &["--rm", "--network", "name=llnet,link-local-ip=169.254.2.11"],
        &["ask", "169.254.2.10:7000"],
    );
    assert!(
        asked.status == Some(0) && asked.stdout.starts_with("ask from 169.254.2.11"),
        "{asked}"
    );
    assert_eq!(exit(&mut srv), Some(0));
    let mut srv = start(
        &home,
        &image,
        &[
            "--name",
            "ll-srv2",
            "--network",
            "name=llnet,link-local-ip=169.254.2.12",
        ],
        &["serve", "7000", "1"],
    );
    let unasked = run_in(
        &home,
        &image,
        &["--rm", "--network", "llnet"],
        &["ask", "169.254.2.12:7000"],
    );
    assert_ne!(unasked.status, Some(0), "{unasked}");
    assert_eq!(shards(&["rm", "-f", "ll-srv2"]).status, Some(0));
    let _ = exit(&mut srv);
    // A network with IPv6: its kernel's link-local address is its MAC's (RFC 4291's
    // modified EUI-64), and a member that was given none reaches another's at it.
    let made = shards(&["network", "create", "--ipv6", "--subnet", "fd7d::/64", "llnet6"]);
    assert_eq!(made.status, Some(0), "{made}");
    let mut srv = start(
        &home,
        &image,
        &[
            "--name",
            "ll-six",
            "--network",
            "name=llnet6,link-local-ip=fe80::2:10",
        ],
        &["serve", "7000", "2"],
    );
    // The address a microVM's kernel makes of its MAC, RFC 4291's modified EUI-64.
    let kernels = |name: &str| {
        let mac = shards(&[
            "exec",
            name,
            "/bin/testguest",
            "fs",
            "print:/sys/class/net/eth0/address",
        ]);
        let o: Vec<u8> = mac
            .stdout
            .trim()
            .split(':')
            .map(|h| u8::from_str_radix(h, 16).unwrap())
            .collect();
        std::net::Ipv6Addr::from([
            0xfe,
            0x80,
            0,
            0,
            0,
            0,
            0,
            0,
            o[0] ^ 2,
            o[1],
            o[2],
            0xff,
            0xfe,
            o[3],
            o[4],
            o[5],
        ])
    };
    let has = shards(&["exec", "ll-six", "/bin/testguest", "addrs"]);
    assert!(
        has.stdout.contains(&format!("eth0 {}/64", kernels("ll-six")))
            && has.stdout.contains("eth0 fe80::2:10/64"),
        "{has}"
    );
    // At once, from its address on the network or its own link-local one, whichever its
    // kernel takes while that one is still being checked (duplicate address detection).
    let asked = run_in(
        &home,
        &image,
        &["--rm", "--network", "llnet6"],
        &["ask", "[fe80::2:10%2]:7000"],
    );
    assert!(
        asked.status == Some(0)
            && (asked.stdout.starts_with("ask from fd7d::") || asked.stdout.starts_with("ask from fe80::")),
        "{asked}"
    );
    // Once its own is checked (no longer tentative in /proc/net/if_inet6, IFA_F_TENTATIVE
    // 0x40): from the address its kernel made of its MAC, which the server's network
    // process knows it by.
    let mut client = start(
        &home,
        &image,
        &["--name", "ll-client", "--network", "llnet6"],
        &["sleep"],
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let table = shards(&[
            "exec",
            "ll-client",
            "/bin/testguest",
            "fs",
            "print:/proc/net/if_inet6",
        ]);
        let checked = table.stdout.lines().any(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.first().is_some_and(|a| a.starts_with("fe80"))
                && f.get(4)
                    .and_then(|x| u8::from_str_radix(x, 16).ok())
                    .is_some_and(|x| x & 0x40 == 0)
        });
        if checked {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "its link-local address stayed tentative: {table}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let asked = shards(&[
        "exec",
        "ll-client",
        "/bin/testguest",
        "ask",
        "[fe80::2:10%2]:7000",
    ]);
    assert!(
        asked.status == Some(0)
            && asked
                .stdout
                .starts_with(&format!("ask from {}", kernels("ll-client"))),
        "{asked}"
    );
    assert_eq!(exit(&mut srv), Some(0));
    assert_eq!(shards(&["rm", "-f", "ll-client"]).status, Some(0));
    let _ = exit(&mut client);
}

/// A microVM's record of the processes its members start and end (D123): its command's
/// exec, a child it forks, that child's exec and its exit code, and its own exit, each the
/// command's, in order, read once the run has ended; 200 children, each over at once,
/// each recorded whole; a child the run's end kills, which takes a while to end after the
/// run's output has; an exec's processes too, followed as they come by `shards watch
/// processes`, which ends with the run.
#[test]
fn a_runs_processes_are_recorded_as_they_start_and_end() {
    use std::io::BufRead as _;
    let Some((home, image)) = home("containers-processes") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let ran = run_in(&home, &image, &["--name", "procs"], &["spawn", "exit", "3"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    let read = shards(&["read", "processes", "procs"]);
    assert_eq!(read.status, Some(0), "{read}");
    // TIME, MEMBER (the command's, one word), PID, EVENT, then what it says.
    let rows: Vec<Vec<&str>> = read
        .stdout
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect())
        .collect();
    let said = |r: &[&str]| r.get(4..).map(|w| w.join(" ")).unwrap_or_default();
    let at = |event: &str, says: &str| {
        rows.iter()
            .position(|r| r.get(1) == Some(&"command") && r.get(3) == Some(&event) && said(r) == says)
            .unwrap_or_else(|| panic!("no {event} {says:?} in\n{}", read.stdout))
    };
    let (own, child) = (
        at("exec", "/bin/testguest spawn exit 3"),
        at("exec", "/bin/testguest exit 3"),
    );
    assert!(own < child, "{}", read.stdout);
    let pid = |row: usize| rows[row][2];
    let (parent, kid) = (pid(own), pid(child));
    let fork = at("fork", &format!("from {parent}"));
    assert!(pid(fork) == kid && own < fork && fork < child, "{}", read.stdout);
    let exited = |p: &str| {
        rows.iter()
            .position(|r| r.get(2) == Some(&p) && r.get(3) == Some(&"exit"))
            .unwrap_or_else(|| panic!("no exit of {p} in\n{}", read.stdout))
    };
    assert_eq!(said(&rows[exited(kid)]), "code 3");
    assert_eq!(said(&rows[exited(parent)]), "code 0");
    assert!(exited(kid) < exited(parent), "{}", read.stdout);
    // Many, one after another, each over at once and reaped by its parent: every one
    // recorded whole, its fork, its command line and its end, which the kernel sends with
    // the exec's event, before the program runs (patches/0002, PM M176).
    let burst = run_in(
        &home,
        &image,
        &["--name", "burst"],
        &["spawns", "200", "exit", "0"],
    );
    assert_eq!(burst.status, Some(0), "{burst}");
    let read = shards(&["read", "processes", "burst"]);
    assert_eq!(read.status, Some(0), "{read}");
    let rows: Vec<Vec<&str>> = read
        .stdout
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect())
        .collect();
    let has = |pid: &str, event: &str, says: &str| {
        rows.iter()
            .any(|r| r.get(2) == Some(&pid) && r.get(3) == Some(&event) && said(r) == says)
    };
    let spawner = rows
        .iter()
        .find(|r| r.get(3) == Some(&"exec") && said(r) == "/bin/testguest spawns 200 exit 0")
        .and_then(|r| r.get(2))
        .unwrap_or_else(|| panic!("no spawner in\n{}", read.stdout));
    for i in 0..200 {
        let pid = rows
            .iter()
            .find(|r| r.get(3) == Some(&"exec") && said(r) == format!("/bin/testguest exit 0 {i}"))
            .and_then(|r| r.get(2))
            .unwrap_or_else(|| panic!("no exec of round {i} in\n{}", read.stdout));
        assert!(
            has(pid, "fork", &format!("from {spawner}")),
            "round {i}:\n{}",
            read.stdout
        );
        assert!(has(pid, "exit", "code 0"), "round {i}:\n{}", read.stdout);
    }
    assert!(has(spawner, "exit", "code 0"), "{}", read.stdout);
    // A process the run's end kills after its command and output have ended, whose end
    // takes freeing what it holds: recorded too, as the run's record waits for it. In the
    // microVM's PID namespace, as `--pid host` leaves it: the end of a namespace's first
    // process waits for the rest of it (kernel/pid_namespace.c, zap_pid_ns_processes).
    let held = run_in(
        &home,
        &image,
        &["--name", "heavy", "-m", "1g", "--pid", "host"],
        &["heavy", "256"],
    );
    assert_eq!(held.status, Some(0), "{held}");
    let read = shards(&["read", "processes", "heavy"]);
    assert_eq!(read.status, Some(0), "{read}");
    let rows: Vec<Vec<&str>> = read
        .stdout
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect())
        .collect();
    let row = |event: &str, says: &str| {
        rows.iter()
            .find(|r| r.get(3) == Some(&event) && said(r) == says)
            .and_then(|r| r.get(2))
            .unwrap_or_else(|| panic!("no {event} {says:?} in\n{}", read.stdout))
    };
    let child = row(
        "fork",
        &format!("from {}", row("exec", "/bin/testguest heavy 256")),
    );
    assert!(
        rows.iter()
            .any(|r| r.get(2) == Some(child) && r.get(3) == Some(&"exit") && said(r) == "signal 9 (SIGKILL)"),
        "{}",
        read.stdout
    );
    // Followed: an exec's, as it comes; the follow ends with the run.
    let mut c = start(&home, &image, &["--name", "watched"], &["sleep"]);
    let mut watcher = common::command()
        .args(["watch", "processes", "watched"])
        .env("SHARDS_HOME", &*home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let out = watcher.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let execed = shards(&["exec", "watched", "/bin/testguest", "exit", "4"]);
    assert_eq!(execed.status, Some(4), "{execed}");
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while !seen
        .iter()
        .any(|l: &String| l.contains(" exit ") && l.ends_with(" code 4"))
    {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => panic!("the exec's exit not followed: {seen:#?}"),
        }
    }
    assert!(
        seen.iter()
            .any(|l| l.contains(" exec ") && l.ends_with("/bin/testguest exit 4")),
        "{seen:#?}"
    );
    assert_eq!(shards(&["rm", "-f", "watched"]).status, Some(0));
    assert!(exit(&mut watcher).is_some(), "the follow ended with the run");
    let _ = exit(&mut c);
}

/// A container given CAP_SYS_ADMIN mounts nothing, (D121), as Docker's default AppArmor profile
/// keeps one where AppArmor is enforced (docker-default, `deny mount,`), which the guest
/// has not: a mount, a remount of a read-only file, the new mount API and its microVM's own
/// shares refused, EACCES, an exec and a joiner too; umount left to it. `apparmor=unconfined`
/// and `--privileged` lift it, as Docker's do (D121); without the capability, Docker's seccomp
/// profile refuses first, EPERM, and under `seccomp=unconfined` the kernel, EPERM.
#[test]
fn a_container_given_sys_admin_mounts_nothing_as_apparmor_keeps_it() {
    let Some((home, image)) = home("containers-mounts") else {
        return;
    };
    let shared = home.join("shared");
    std::fs::create_dir(&shared).unwrap();
    let bind = format!("{}:/data", shared.display());
    let fs = |options: &[&str], op: &str| run_in(&home, &image, options, &["fs", op]);
    let admin = ["--rm", "-u", "0", "--cap-add", "SYS_ADMIN"];
    let refused = |options: &[&str], op: &str, said: &str| {
        let r = fs(options, op);
        assert!(
            r.status == Some(1) && r.stderr.contains(&format!("{op}: {said}")),
            "{options:?} {op}: {r}"
        );
    };
    for op in [
        "mount:tmpfs:none:/mnt",
        "fsopen:tmpfs",
        "open-tree:/etc",
        "remount-rw:/etc/hostname",
    ] {
        refused(&admin, op, "Permission denied");
    }
    // Its microVM's own share, whose tag it knows: a host's directory.
    let with_share: Vec<&str> = admin.iter().copied().chain(["-v", bind.as_str()]).collect();
    refused(&with_share, "mount:virtiofs:shards0:/mnt", "Permission denied");
    // umount is left to it, as docker-default leaves it (D121): a tmpfs of its own.
    let with_tmpfs: Vec<&str> = admin.iter().copied().chain(["--tmpfs", "/t"]).collect();
    let unmounted = fs(&with_tmpfs, "umount:/t");
    assert_eq!(unmounted.status, Some(0), "{unmounted}");
    // Lifted as Docker's is (D121).
    for options in [
        &[
            "--rm",
            "-u",
            "0",
            "--cap-add",
            "SYS_ADMIN",
            "--security-opt",
            "apparmor=unconfined",
        ][..],
        &["--rm", "-u", "0", "--privileged"][..],
    ] {
        let made = run_in(&home, &image, options, &["fs", "mkdir:/m", "mount:tmpfs:none:/m"]);
        assert_eq!(made.status, Some(0), "{options:?}: {made}");
    }
    // Without the capability, Docker's seccomp profile refuses first; under
    // `seccomp=unconfined`, the kernel, EPERM, where AppArmor would answer mount(2) EACCES
    // (PM M174): no filter for an errno alone on a container asked to have none (D121).
    refused(
        &["--rm", "-u", "0"],
        "mount:tmpfs:none:/mnt",
        "Operation not permitted",
    );
    let no_seccomp = ["--rm", "-u", "0", "--security-opt", "seccomp=unconfined"];
    // Its target made first: the kernel looks it up before it checks the capability.
    let r = run_in(
        &home,
        &image,
        &no_seccomp,
        &["fs", "mkdir:/m", "mount:tmpfs:none:/m"],
    );
    assert!(
        r.status == Some(1) && r.stderr.contains("mount:tmpfs:none:/m: Operation not permitted"),
        "{r}"
    );
    for op in ["fsopen:tmpfs", "open-tree:/etc"] {
        refused(&no_seccomp, op, "Operation not permitted");
    }
    // An exec takes it, and a joiner.
    let up = run_in(
        &home,
        &image,
        &["-d", "--name", "admin", "--cap-add", "SYS_ADMIN"],
        &["sleep"],
    );
    assert_eq!(up.status, Some(0), "{up}");
    let exec = shards_in(
        &home,
        &[
            "exec",
            "-u",
            "0",
            "admin",
            "/bin/testguest",
            "fs",
            "mount:tmpfs:none:/mnt",
        ],
    );
    assert!(exec.stderr.contains("Permission denied"), "{exec}");
    refused(
        &[
            "--rm",
            "-u",
            "0",
            "--cap-add",
            "SYS_ADMIN",
            "--network",
            "container:admin",
        ],
        "mount:tmpfs:none:/mnt",
        "Permission denied",
    );
    assert_eq!(shards_in(&home, &["rm", "-f", "admin"]).status, Some(0));
}

/// `start -a` passes signals on as docker/cli's start does (D160): to a container without a
/// terminal, whose command hears them; not to one with a terminal, whose own keys are its
/// signals: the client ends by the signal alone, the container running on.
#[test]
fn start_attached_passes_signals_on() {
    let Some((home, image)) = home("containers-start-signals") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    // SIGTERM on Unix; Windows' interrupt, SIGINT (common::terminate).
    let (trapped, number) = common::PROXIED;
    for (name, options) in [("plain", &[][..]), ("terminal", &["-t"][..])] {
        let mut args = vec!["create", "--pull", "never", "--name", name];
        args.extend(options);
        args.extend([image.as_str(), "trap", trapped]);
        let made = shards(&args);
        assert_eq!(made.status, Some(0), "{made}");
    }
    let attached = |name: &str| -> Child {
        let mut child = common::interruptible(
            common::command()
                .args(["start", "-a", name])
                .env("SHARDS_HOME", &*home)
                .env("SHARDS_KERNEL", kernel())
                .env("SHARDS_INIT", guest_init())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .spawn()
        .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("ready"), "{name}: {line:?}");
        child
    };
    let got = format!("got {number}");
    let mut plain = attached("plain");
    common::terminate(&plain);
    assert_eq!(exit(&mut plain), Some(0));
    let logged = shards(&["logs", "plain"]);
    assert!(logged.stdout.contains(&got), "{logged}");
    let mut terminal = attached("terminal");
    common::terminate(&terminal);
    assert_eq!(
        exit(&mut terminal),
        common::TERMINATED,
        "the client ends by the signal"
    );
    let state = shards(&["inspect", "-f", "{{.State.Status}}", "terminal"]);
    assert_eq!(state.stdout, "running\n", "{state}");
    let logged = shards(&["logs", "terminal"]);
    assert!(!logged.stdout.contains(&got), "{logged}");
    assert_eq!(shards(&["rm", "-f", "terminal"]).status, Some(0));
}

/// A stopped container's writable layer stays in its own file (D190): an ext4 file system
/// whose journal its guest wrote in place as its run ended, nothing of it left to replay,
/// holding what its run wrote and its whiteouts, which its next run puts back. One whose
/// microVM was killed holds what its guest committed: its journal is replayed on the host,
/// and a visit to its files finds them.
#[test]
fn a_stopped_microvms_layer_stays_in_its_file() {
    use shards_ext4::journal;
    use shards_ext4::read::{Kind, Volume};
    let Some((home, image)) = home("containers-kept") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let first = run_in(
        &home,
        &image,
        &["--name", "kept", "-u", "0"],
        &["fs", "write:/kept=one", "rm:/etc/group"],
    );
    assert_eq!(first.status, Some(0), "{first}");
    let dir_of = |name: &str| {
        let id = shards(&["inspect", "-f", "{{.Id}}", name]);
        home.join("containers").join(id.stdout.trim())
    };
    // Kept once its microVM has gone, after its end is told.
    let settled = |dir: &Path| {
        let deadline = Instant::now() + TIMEOUT;
        while !(dir.join("layer.ext4").exists() && !dir.join("layer.new").exists()) {
            assert!(Instant::now() < deadline, "no layer kept in {}", dir.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let dir = dir_of("kept");
    settled(&dir);
    assert!(!dir.join("layer.tar").exists());
    let file = std::fs::File::open(dir.join("layer.ext4")).unwrap();
    assert!(
        !journal::needs_replay(&file).unwrap(),
        "its journal written in place as its run ended"
    );
    let v = Volume::open(file).unwrap();
    let kept = v.lookup(b"/upper/kept").unwrap().unwrap();
    let mut bytes = vec![0u8; kept.size as usize];
    v.read_at(&kept, 0, &mut bytes).unwrap();
    assert_eq!(bytes, b"one");
    let gone = v.lookup(b"/upper/etc/group").unwrap().unwrap();
    assert_eq!((gone.kind, gone.device()), (Kind::Char, (0, 0)), "a whiteout");
    // Its next run finds /etc/group gone, as its first left it.
    let again = shards(&["start", "-a", "kept"]);
    assert_eq!(again.status, Some(1), "{again}");
    assert!(again.stderr.contains("rm:/etc/group: No such file"), "{again}");

    // Killed once its guest has committed what it wrote: its journal still to replay.
    let killed = run_in(
        &home,
        &image,
        &["-d", "--name", "killed", "-u", "0"],
        // A line, which stdout passes on as it ends (Rust's LineWriter).
        &[
            "fs",
            "write:/crashed=two\n",
            "sync",
            "print:/crashed",
            "sleep:600000",
        ],
    );
    assert_eq!(killed.status, Some(0), "{killed}");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let logs = shards(&["logs", "killed"]);
        if logs.stdout == "two\n" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run never committed its file: {logs}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid: i32 = shards(&["inspect", "-f", "{{.State.Pid}}", "killed"])
        .stdout
        .trim()
        .parse()
        .unwrap();
    common::kill_now(pid);
    let dir = dir_of("killed");
    settled(&dir);
    let file = std::fs::File::open(dir.join("layer.ext4")).unwrap();
    assert!(
        journal::needs_replay(&file).unwrap(),
        "killed with what its guest committed in its journal"
    );
    drop(file);
    let diff = shards(&["diff", "killed"]);
    assert_eq!(diff.status, Some(0), "{diff}");
    assert!(diff.stdout.lines().any(|l| l == "A /crashed"), "{diff}");
    let file = std::fs::File::open(dir.join("layer.ext4")).unwrap();
    assert!(!journal::needs_replay(&file).unwrap(), "replayed");
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    assert!(log.contains("transactions of its journal"), "{log}");
    assert_eq!(shards(&["rm", "-f", "kept", "killed"]).status, Some(0));
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
            "shards: No such container: nobody\nfailed to start containers: nobody\n"
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
    // Its base's variant kept, as dockerd's child image keeps it (moby docker-v29.3.1
    // daemon/internal/image NewChildImage): of this host's own platform, here.
    let (_, blobs) = common::test_image_with(Some(b"variant"));
    let (arch, variant) = if cfg!(target_arch = "aarch64") {
        ("arm64", "v8")
    } else {
        ("amd64", "v1")
    };
    let varied = String::from_utf8(blobs[0].clone())
        .unwrap()
        .replace(
            &format!(r#""architecture":"{arch}""#),
            &format!(r#""architecture":"{arch}","variant":"{variant}""#),
        )
        .into_bytes();
    assert_ne!(varied, blobs[0], "the config named the host's architecture");
    let archive = common::tar(&[
        ("cfg.json", 0o644, 0, Some(varied.as_slice())),
        ("vvv", 0o755, 0, None),
        ("vvv/layer.tar", 0o644, 0, Some(blobs[1].as_slice())),
        (
            "manifest.json",
            0o644,
            0,
            Some(br#"[{"Config":"cfg.json","RepoTags":["varied:1"],"Layers":["vvv/layer.tar"]}]"#),
        ),
    ]);
    let archive_path = home.join("varied.tar");
    std::fs::write(&archive_path, archive).unwrap();
    assert_eq!(
        shards(&["load", "-i", archive_path.to_str().unwrap()]).status,
        Some(0)
    );
    let ran = run_in(&home, "varied:1", &["--name", "varied"], &["exit", "0"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    assert_eq!(
        shards(&["commit", "varied", "test/committed:variant"]).status,
        Some(0)
    );
    let kept = shards(&[
        "image",
        "inspect",
        "--format",
        "{{.Variant}}",
        "test/committed:variant",
    ]);
    assert_eq!(kept.stdout.trim(), variant, "{kept}");
    assert_eq!(shards(&["rm", "varied"]).status, Some(0));
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
        (Some(1), "--pause and --no-pause: give one\n"),
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
    assert!(refused.stderr.contains("it is running"), "{}", refused.stderr);
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
fn ps_filters_microvms() {
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
    // Refused in dockerd's words (D27).
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
            (Some(1), format!("shards: {said}\n").as_str()),
            "{refused}"
        );
    }
    assert_eq!(shards(&["rm", "-f", "up"]).status, Some(0));
    exit(&mut sleeper);
}

#[test]
fn images_and_prunes_filter() {
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
            (Some(1), format!("shards: {said}\n").as_str()),
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
        twice.stderr, "shards: more than one until filter specified\n",
        "{twice}"
    );
    let wrong = shards(&["system", "prune", "-f", "--filter", "dangling=true"]);
    assert_eq!(wrong.stderr, "shards: invalid filter 'dangling'\n", "{wrong}");
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
fn inspect_describes_microvms_and_images() {
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
    // What is not there, in the CLI's and dockerd's words (D27).
    let none = shards(&["inspect", "nosuch"]);
    assert_eq!(
        (none.status, none.stdout.as_str(), none.stderr.as_str()),
        (Some(1), "[]\n", "error: no such object: nosuch\n"),
        "{none}"
    );
    let volume = shards(&["inspect", "--type", "volume", "v"]);
    assert_eq!(volume.stderr, "shards: no volume is named v\n", "{volume}");
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
fn run_labels_names_and_resolves() {
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

/// `--cidfile`, `-q`, `--platform` and `--sig-proxy`, as docker run and create take them (D27):
/// the ID written once the microVM is made, and the file left alone or removed as
/// docker/cli's cidFile does (D27); a pull said nothing of; the platform read as containerd
/// reads it, and one this host's microVMs cannot run refused before anything is pulled;
/// signals kept from the command.
#[test]
fn run_writes_cidfiles_pulls_quietly_and_keeps_signals() {
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
    // A directory that is not there, in the host's own words: toStatusError's 127 for
    // Unix's, and 125 for Windows', whose "path" it does not look for.
    let lost = home.join("nowhere/x.cid");
    let missing = run_in(
        &home,
        &image,
        &["--cidfile", lost.to_str().unwrap()],
        &["exit", "0"],
    );
    let (status, why) = if cfg!(windows) {
        (125, "The system cannot find the path specified.")
    } else {
        (127, "no such file or directory")
    };
    assert_eq!(missing.status, Some(status), "{missing}");
    assert!(
        missing.stderr.starts_with(&format!(
            "shards: failed to create the container ID file: open {}: {why}\n",
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
    // This host's platform, however it is spelled, runs; one no emulator runs either is
    // refused (another CPU's runs under its emulator: images.rs,
    // a_run_of_another_cpus_image_runs_under_its_emulator), and a specifier containerd
    // cannot read is said as it says it.
    let (ours, other) = if std::env::consts::ARCH == "aarch64" {
        ("linux/arm64/v8", "linux/sparc64")
    } else {
        ("linux/amd64", "linux/sparc64")
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
    let (fresh, _) = served_counting();
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
    common::terminate(&kept);
    assert_eq!(exit(&mut kept), common::TERMINATED);
    let state = shards(&["inspect", "-f", "{{.State.Status}}", "unproxied"]);
    assert_eq!(state.stdout, "running\n", "{state}");
    let removed = shards(&["rm", "-f", "unproxied"]);
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// `run`'s memory and CPU flags as docker run takes them: checked as dockerd checks them,
/// its warnings said, the microVM growing to hold them (D148), and the workload's cgroup limited
/// as runc limits it, seen read-only at /sys/fs/cgroup as a container sees its own; a
/// workload past its memory killed, and OOMKilled, as Docker reports it (D148).
#[test]
fn run_limits_a_microvms_resources() {
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
    // Its swappiness is its microVM's own (D159): nothing is discarded, and nothing warned.
    assert!(!limited.stderr.contains("WARNING"), "{limited}");
    let read: Vec<&str> = limited
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("= "))
        .collect();
    // Its microVM has every CPU of the host's, and its workload the one it names (D191).
    let host = std::thread::available_parallelism().unwrap().get();
    let online = if host == 1 {
        "0\\n".to_string()
    } else {
        format!("0-{}\\n", host - 1)
    };
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
            online.as_str(),
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
        shown.stdout, "67108864 134217728 33554432 1500000000 512 50 0 50\n",
        "{shown}"
    );
    // JSON's names fail on the Go types, and are read from the JSON, where a null is no
    // value, as docker/cli's inspector falls back (D27).
    let raw = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.CpuShares}} {{.HostConfig.MemorySwappiness}}",
        "limited",
    ]);
    assert_eq!(raw.stdout, "512 50\n", "{raw}");
    // The microVM holds what the limit allows: what the kernel says may be allocated, and
    // the free pages its CPUs' lists hold, which it leaves out of that (M179), is at least
    // the limit, across the sizes where its own share steps up, as a run's guest keeps it;
    // with one CPU and with every one, its microVM having all the host's either way, each
    // vCPU keeping memory of its own (M183, D191).
    let host_cpus = std::thread::available_parallelism().map_or(1, usize::from);
    // Without a limit, every CPU of the host's, as Docker's container has (D125).
    let online = run_in(
        &home,
        &image,
        &["--rm"],
        &["stat", "/sys/devices/system/cpu/online"],
    );
    let want = if host_cpus == 1 {
        "= 0\\n".to_string()
    } else {
        format!("= 0-{}\\n", host_cpus - 1)
    };
    assert!(
        online.stdout.lines().any(|l| l == want),
        "{host_cpus} CPUs: {online}"
    );
    let every = host_cpus.to_string();
    for (limit, cpus) in [
        ("700m", "1"),
        ("2500m", "1"),
        ("700m", every.as_str()),
        ("2500m", every.as_str()),
    ] {
        // It holds all but what its own process and page tables take of its limit, in a
        // microVM that boots with less and plugs the rest as it needs it (D148). Plugging
        // and touching gigabytes is far slower than a start on a contended CI runner, so
        // these runs get a timeout of their own, well past TIMEOUT (measured 199 s for the
        // whole test under 32 busy loops; it progresses, it never hangs).
        let limit_mib = limit.trim_end_matches('m').parse::<u64>().unwrap();
        let wanted = (limit_mib - 16).to_string();
        let held = run_in_within(
            &home,
            &image,
            &["--rm", "-m", limit, "--cpus", cpus],
            &["alloc", &wanted],
            Duration::from_secs(240),
        );
        assert_eq!(
            held.status,
            Some(0),
            "{limit}, --cpus {cpus}: {held}{}",
            oom_said(&home)
        );
        assert!(
            held.stdout.contains(&format!("allocated {wanted}")),
            "{limit}, --cpus {cpus}: {held}"
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
    // Its refusals, in shards' words (D158): `run` with the CLI's help line and 125;
    // `create` as they are.
    let small = run_in(&home, &image, &["-m", "4m"], &["exit", "0"]);
    assert_eq!(
        (small.status, small.stderr.as_str()),
        (
            Some(125),
            "shards: --memory 4194304: a run's memory is at least 6 MiB\n\nRun 'shards run --help' for more information\n"
        ),
        "{small}"
    );
    let made = shards(&["create", "--pull", "never", "-m", "4m", &image]);
    assert_eq!(
        (made.status, made.stderr.as_str()),
        (
            Some(1),
            "shards: --memory 4194304: a run's memory is at least 6 MiB\n"
        ),
        "{made}"
    );
    let cpus = std::thread::available_parallelism().unwrap().get();
    let many = run_in(&home, &image, &["--cpus", "100000"], &["exit", "0"]);
    assert_eq!(many.status, Some(125), "{many}");
    assert!(
        many.stderr
            .contains(&format!("--cpus 100000.00: from 0.01 to {cpus}, the host's CPUs")),
        "{many}"
    );
    let swappy = run_in(&home, &image, &["--memory-swappiness", "101"], &["exit", "0"]);
    assert_eq!(
        (swappy.status, swappy.stderr.as_str()),
        (
            Some(125),
            "shards: --memory-swappiness 101: from 0 to 100, or -1 to leave the microVM's own\n\nRun 'shards run --help' for more information\n"
        ),
        "{swappy}"
    );
}

/// `--read-only`, `--tmpfs`, `--shm-size`, `--ulimit` and `--sysctl` (D38), as docker run takes
/// them: the workload's root read-only and its tmpfs mounts in its own mount namespace,
/// which an exec joins; its rlimits, which an exec takes too; its sysctls set; inspect
/// as dockerd keeps them (D38); and dockerd's and runc's refusals.
#[test]
fn run_sets_up_mounts_limits_and_sysctls() {
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
        (Some(1), "shards: container rootfs is marked read-only\n"),
        "{copied}"
    );
    // Its tmpfs takes it, where its command finds it: a copy is made on the mounts its
    // command sees. dockerd refuses it as the root's (checkWritablePath: a `--tmpfs` is
    // none of its mount points), and its copies write a tmpfs of their own, which the
    // container never sees (containerfs_linux.go, containerFSView).
    let copied = shards(&["cp", file.to_str().unwrap(), "sealed:/run/"]);
    assert_eq!(copied.status, Some(0), "{copied}");
    let found = exec(&["stat", "/run/copied.txt"]);
    assert!(found.stdout.contains("\n= x\n"), "{found}");
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
            (Some(status), format!("shards: {words}{help}").as_str()),
            "{options:?}"
        );
    }
}

/// `--cap-add`, `--cap-drop`, `--group-add`, `--oom-score-adj` and `--privileged` (D42), as docker
/// run takes them, and `exec --privileged`: capabilities as moby tweaks them (D42), groups as
/// moby adds them, the OOM score set before the command, an exec taking the workload's;
/// a privileged microVM unmasked, its /sys writable, every capability; dockerd's refusals.
#[test]
fn run_sets_capabilities_groups_and_privileges() {
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
            (Some(125), format!("shards: {words}{help}").as_str()),
            "{options:?}"
        );
    }
}

#[test]
fn info_and_disk_usage_format() {
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
fn ps_lists_microvms() {
    let Some((home, image)) = home("containers-ps") else {
        return;
    };
    assert_eq!(
        run_in(&home, &image, &["--name", "done"], &["exit", "0"]).status,
        Some(0)
    );
    // Under --init, so that it ends of the SIGTERM it does not handle, which as PID 1 it
    // would not hear.
    let mut sleeper = start(&home, &image, &["--name", "up", "--init"], &["sleep"]);
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
    // Its stdin, without `-i`, is /dev/null, as Docker's is (D160); with it, a pipe.
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
            "shards: --since x: failed to parse value as time or duration: \"x\"\n",
        ),
        (
            "2013-13-01",
            "shards: --since 2013-13-01: parsing time \"2013-13-01\": month out of range\n",
        ),
        (
            "1.+5",
            "shards: --since 1.+5: invalid timestamp \"1.+5\": invalid nanoseconds: invalid character '+' at position 0\n",
        ),
    ] {
        let bad = shards_in(&home, &["logs", "--since", window, "reporter"]);
        assert_eq!((bad.status, bad.stderr.as_str()), (Some(1), said), "{window}");
    }
    // The container is found first, as the CLI inspects it first.
    let missing = shards_in(&home, &["logs", "--since", "x", "nosuch"]);
    assert_eq!(missing.stderr, "shards: No such container: nosuch\n");

    let errs = run_in(&home, &image, &["--name", "errs"], &["stderr", "to stderr"]);
    assert_eq!(errs.status, Some(0), "{}", errs.stderr);
    let logs = shards_in(&home, &["logs", "errs"]);
    assert_eq!((logs.stdout.as_str(), logs.stderr.as_str()), ("", "to stderr"));

    // Under --init, so that it ends of the SIGTERM it does not handle, which as PID 1 it
    // would not hear.
    let mut sleeper = start(&home, &image, &["--name", "follow", "--init"], &["sleep"]);
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

/// An exec into a microVM whose command nests its own cgroups, as a nested runtime does
/// (BuildKit's buildkitd, which buildx runs as a builder, and Docker-in-Docker): the
/// workload's cgroup then takes no process, and the exec joins the one its command moved
/// into, as runc's exec joins container init's (D173; runc#2356).
#[test]
fn an_exec_joins_a_workload_that_nests_its_cgroups() {
    let Some((home, image)) = home("containers-nest") else {
        return;
    };
    let run = run_in(
        &home,
        &image,
        &["-d", "--name", "nester", "--privileged", "-u", "0"],
        &["nest"],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let mut said = String::new();
    let nested = (0..400).any(|_| {
        said = shards_in(&home, &["logs", "nester"]).stdout;
        said.contains("nested") || {
            std::thread::sleep(Duration::from_millis(25));
            false
        }
    });
    assert!(nested, "{said}");
    let exec = shards_in(
        &home,
        &["exec", "nester", "/bin/testguest", "slurp", "/proc/self/cgroup"],
    );
    assert_eq!(
        (exec.status, exec.stdout.as_str()),
        (Some(0), "slurp /proc/self/cgroup: 0::/init\n"),
        "{exec}"
    );
    let removed = shards_in(&home, &["rm", "-f", "nester"]);
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// A runtime nested in a microVM keeps overlays on its volumes (D174), as BuildKit's
/// snapshots and Docker in Docker's layers are kept: an overlay whose upper and work
/// directories are on a volume mounts read-write; what it writes is its upper layer's;
/// what it removes of the lower is a whiteout on the volume; and a directory removed and
/// made again hides the lower's (an opaque directory, by its upper layer's attribute).
#[test]
fn an_overlay_on_a_volume_is_written_as_on_a_kernels_own_filesystem() {
    let Some((home, image)) = home("containers-overlay") else {
        return;
    };
    let run = run_in(
        &home,
        &image,
        &[
            "-d",
            "--name",
            "layered",
            "--privileged",
            "-u",
            "0",
            "-v",
            "layers:/data",
        ],
        &["sleep"],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let exec = |args: &[&str]| {
        let mut all = vec!["exec", "layered", "/bin/testguest"];
        all.extend_from_slice(args);
        shards_in(&home, &all)
    };
    let made = exec(&[
        "fs",
        "mkdir:/data/lower",
        "mkdir:/data/lower/dir",
        "write:/data/lower/gone=1",
        "write:/data/lower/kept=1",
        "write:/data/lower/dir/old=1",
        "mkdir:/data/upper",
        "mkdir:/data/work",
        "mkdir:/merged",
        "overlay:/data/lower:/data/upper:/data/work:/merged",
        "write:/merged/made=1",
        "rm:/merged/gone",
        "rmdir:/merged/dir",
        "mkdir:/merged/dir",
        "write:/merged/dir/new=1",
    ]);
    assert_eq!(
        made.status,
        Some(0),
        "{made}\nthe kernel: {}",
        exec(&["kernlog", "overlay"]).stdout
    );
    let seen = exec(&[
        "slurp",
        "/merged/made",
        "/merged/kept",
        "/merged/gone",
        "/merged/dir/old",
        "/merged/dir/new",
        "/data/upper/made",
    ]);
    assert_eq!(
        seen.stdout,
        "slurp /merged/made: 1\nslurp /merged/kept: 1\nslurp /merged/gone: errno 2\nslurp /merged/dir/old: errno 2\nslurp /merged/dir/new: 1\nslurp /data/upper/made: 1\n",
        "{seen}"
    );
    let whiteout = exec(&["devnode", "/data/upper/gone"]);
    assert_eq!(
        whiteout.stdout,
        "/data/upper/gone char 0:0\n",
        "{whiteout}\nthe volume: {}\nthe kernel: {}",
        exec(&["tree", "/data"]).stdout,
        exec(&["kernlog", "overlay"]).stdout
    );
    // Nothing overlayfs had to do without: no read-only mount, no whiteout of its own for
    // each removal (its shared one's links reached).
    let said = exec(&["kernlog", "overlay"]).stdout;
    assert!(
        !said.contains("read-only") && !said.contains("missing") && !said.contains("Failed"),
        "{said}"
    );
    let removed = shards_in(&home, &["rm", "-f", "layered"]);
    assert_eq!(removed.status, Some(0), "{removed}");
}

#[test]
fn detached_runs_print_their_id_and_go_on() {
    let Some((home, image)) = home("containers-detached") else {
        return;
    };
    // Under --init, so that it ends of the SIGTERM it does not handle, which as PID 1 it
    // would not hear.
    let run = run_in(&home, &image, &["-d", "--name", "bg", "--init"], &["sleep"]);
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
    // Its host is named for its ID, as Docker names a container's (D27).
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
fn a_command_that_cannot_start_says_why() {
    let Some((home, image)) = home("containers-unstarted") else {
        return;
    };
    let said = "shards: exec: \"/nonexistent\": stat /nonexistent: no such file or directory\n\nRun 'shards run --help' for more information\n";
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

/// `-h` is help on every form of shards' grammar, and each form's page is its own: named
/// as it is said (`shards list vm`), in shards' words, never a command of Docker's
/// (`Usage:  shards ps`, `docker cp`).
#[test]
fn every_form_of_shards_grammar_answers_help_with_its_own_page() {
    let home = TempDir::new("containers-forms");
    for (_, action, takes, _) in shards_cmdline::grammar::USES {
        for alternative in takes.split(" | ") {
            let thing = alternative.split(' ').next().unwrap();
            for help in ["-h", "--help"] {
                let r = without_vms(&home, &[action, thing, help]);
                let said = format!("shards {action} {thing}");
                assert_eq!(r.status, Some(0), "{said} {help}: {r}");
                assert!(r.stdout.contains(&said), "{said} {help}: {r}");
                assert!(!r.stdout.contains("docker "), "{said} {help}: {r}");
                assert!(!r.stdout.contains("container"), "{said} {help}: {r}");
            }
        }
    }
    // What an option does, as shards does it, where Docker's words would say otherwise (D158);
    // Docker's own command keeps Docker's words.
    let page = without_vms(&home, &["create", "vm", "--help"]).stdout;
    for said in [
        "Give the command one of its microVM's devices, the VM's, never the host's",
        "Add an annotation to the microVM, kept with it and shown by inspect",
    ] {
        assert!(
            page.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(said),
            "{said}: {page}"
        );
    }
    assert!(
        !page.contains("OCI runtime") && !page.contains("host device"),
        "{page}"
    );
    let docker = without_vms(&home, &["create", "--help"]).stdout;
    assert!(docker.contains("Add a host device to the container"), "{docker}");
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
        !home.join("requests").exists() && !home.join("daemon.pid").exists(),
        "no daemon was started"
    );
}

/// This build's `shards`, whose runs' VMs wait before they are ready for as long as the
/// test keeps a gate shut, so that a run stays pending, its container created (audit A06).
/// On Unix it is in a directory of its own, with a VM process binary (`SHARDS_VM_BINARY`)
/// that waits for the gate to open before it becomes this build's. Windows has no exec,
/// and a VM process opens nothing but what its daemon opens for it (D30): there the gate
/// is a copy of the init its runs boot, held open sharing nothing under an oplock
/// ([`Oplocked`]), so that the daemon's open of it for a VM waits until the gate opens.
/// Not the kernel's: the daemon reads that as it makes the run, before its container, for
/// the version its seccomp profile is held to (`guest::version_of`).
struct Gated {
    // Before the directory: the gate opens before its init is removed.
    #[cfg(windows)]
    held: std::cell::RefCell<Option<Oplocked>>,
    dir: TempDir,
}

impl Gated {
    #[cfg(unix)]
    fn new(name: &str) -> Gated {
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
        // Exec'd by the daemon's spawner.
        common::program(&vm, &script);
        Gated { dir }
    }

    #[cfg(windows)]
    fn new(name: &str) -> Gated {
        let dir = TempDir::new(name);
        std::fs::copy(guest_init(), dir.join("init")).unwrap();
        let held = Oplocked::new(&dir.join("init"));
        Gated {
            held: std::cell::RefCell::new(Some(held)),
            dir,
        }
    }

    #[cfg(unix)]
    fn open(&self) {
        std::fs::write(self.dir.join("open"), b"").unwrap();
    }

    #[cfg(windows)]
    fn open(&self) {
        drop(self.held.borrow_mut().take());
    }

    /// The command that runs this build's `shards`: its copy, beside its VM process
    /// binary, on Unix.
    fn command(&self) -> Command {
        #[cfg(unix)]
        {
            let mut command = Command::new(self.dir.join("shards"));
            command.env("SHARDS_VM_BINARY", self.dir.join("shards-vm"));
            command
        }
        #[cfg(windows)]
        Command::new(common::shards())
    }

    /// The init its runs boot: the gated copy, on Windows.
    fn init(&self) -> std::path::PathBuf {
        if cfg!(windows) {
            self.dir.join("init")
        } else {
            guest_init().to_path_buf()
        }
    }

    /// `shards ARGS` from this directory in `home`, whose runs boot the test kernel and
    /// init, left going.
    fn start(&self, home: &Path, args: &[&str]) -> Going {
        // Its copy of shards was written a moment ago.
        let mut child = common::spawn(
            common::as_tests_run(&mut self.command())
                .args(args)
                .env("SHARDS_HOME", home)
                .env("SHARDS_KERNEL", kernel())
                .env("SHARDS_INIT", self.init())
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

/// A file held open sharing nothing under a Read-Handle oplock (FSCTL_REQUEST_OPLOCK): an
/// open of it by another handle that reads, writes or deletes it, which its sharing
/// refuses, breaks the oplock and waits for the break to be acknowledged, which closing
/// the oplock's handle does as this drops. The wait has no end: "the system would wait
/// forever to receive the acknowledgment as there's no timeout" ("Breaking Oplocks"); "the
/// open operation is blocked and the Read-Handle oplock breaks. The application that owns
/// the oplock may acknowledge the break by closing the handle" ("Types of Opportunistic
/// Locks"). An open that reads its attributes alone shares nothing and waits for nothing.
#[cfg(windows)]
struct Oplocked {
    file: Option<std::fs::File>,
    // Written as the request completes, which may be after it is made: boxed, not to move,
    // and kept until it has.
    overlapped: Box<windows_sys::Win32::System::IO::OVERLAPPED>,
    _answer: Box<windows_sys::Win32::System::Ioctl::REQUEST_OPLOCK_OUTPUT_BUFFER>,
    _event: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl Oplocked {
    fn new(path: &Path) -> Oplocked {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
        use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_SHARING_VIOLATION};
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
        use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};
        use windows_sys::Win32::System::Ioctl::{
            FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_HANDLE, OPLOCK_LEVEL_CACHE_READ,
            REQUEST_OPLOCK_CURRENT_VERSION, REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
            REQUEST_OPLOCK_OUTPUT_BUFFER,
        };
        use windows_sys::Win32::System::Threading::CreateEventW;
        // A handle that shares nothing is refused while another reads the file: Defender's,
        // scanning the copy just written, is waited out.
        let deadline = Instant::now() + TIMEOUT;
        let file = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(path)
            {
                Ok(file) => break file,
                Err(e)
                    if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed())
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{}: {e}", path.display()),
            }
        };
        // SAFETY: an unnamed manual-reset event, owned from here.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null(), "an event: {}", std::io::Error::last_os_error());
        // SAFETY: the event just made, this alone holds.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        // SAFETY: OVERLAPPED and REQUEST_OPLOCK_OUTPUT_BUFFER are plain data, zeroes valid
        // values of them.
        let (mut overlapped, mut answer): (Box<OVERLAPPED>, Box<REQUEST_OPLOCK_OUTPUT_BUFFER>) =
            unsafe { (Box::new(std::mem::zeroed()), Box::new(std::mem::zeroed())) };
        overlapped.hEvent = event.as_raw_handle();
        let version = u16::try_from(REQUEST_OPLOCK_CURRENT_VERSION).unwrap();
        answer.StructureVersion = version;
        answer.StructureLength = u16::try_from(size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>()).unwrap();
        let asked = REQUEST_OPLOCK_INPUT_BUFFER {
            StructureVersion: version,
            StructureLength: u16::try_from(size_of::<REQUEST_OPLOCK_INPUT_BUFFER>()).unwrap(),
            RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE,
            Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
        };
        // SAFETY: the request is copied in as it is made (METHOD_BUFFERED), and its answer
        // and OVERLAPPED live until it has completed, which dropping this waits for.
        let done = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_REQUEST_OPLOCK,
                (&raw const asked).cast(),
                u32::try_from(size_of::<REQUEST_OPLOCK_INPUT_BUFFER>()).unwrap(),
                (&raw mut *answer).cast(),
                u32::try_from(size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>()).unwrap(),
                std::ptr::null_mut(),
                &raw mut *overlapped,
            )
        };
        // Granted, the request pends until the oplock breaks.
        let e = std::io::Error::last_os_error();
        assert!(
            done == 0 && e.raw_os_error() == Some(ERROR_IO_PENDING.cast_signed()),
            "an oplock on {}: {e}",
            path.display()
        );
        Oplocked {
            file: Some(file),
            overlapped,
            _answer: answer,
            _event: event,
        }
    }
}

#[cfg(windows)]
impl Drop for Oplocked {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult};
        let Some(file) = self.file.take() else {
            return;
        };
        // The request, still pending or completed by a break, ended and waited for before
        // its buffers go; then the handle is closed, which acknowledges a break, and an open
        // that waits on it goes on.
        // SAFETY: the request made on this handle with this OVERLAPPED.
        unsafe {
            CancelIoEx(file.as_raw_handle(), &raw const *self.overlapped);
            let mut moved = 0;
            GetOverlappedResult(
                file.as_raw_handle(),
                &raw const *self.overlapped,
                &raw mut moved,
                1,
            );
        }
        drop(file);
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
            // Where the run stands: what the daemon said, and its processes.
            let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(40).collect();
            let procs = common::processes_under(&log);
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
            || waited.status == Some(1) && waited.stderr == "shards: No such container: racer\n",
        "{waited}"
    );
    gated.open();
    let run = run.finish();
    assert_eq!(run.status, Some(125), "{run}");
    assert_eq!(
        untimed(&run.stderr),
        format!("shards: No such container: {id}\n\nRun 'shards run --help' for more information\n"),
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
    common::kill_now(pid);
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
        "shards: the daemon is shutting down\n\nRun 'shards run --help' for more information\n",
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

/// Segment `seq`'s log in container directory `dir`, as written: its file as written, or,
/// once the daemon has compressed it (D162), every zstd frame of its compressed file, the
/// seek table's skippable frame passed over. The file as written goes only once the
/// compressed one is whole, so it is read first.
fn segment_log(dir: &Path, seq: u64) -> Vec<u8> {
    use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
    use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
    match std::fs::read(dir.join(format!("log.{seq}"))) {
        Ok(written) => return written,
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}"),
    }
    let packed = std::fs::read(dir.join(format!("log.{seq}.zst"))).unwrap();
    let mut src = packed.as_slice();
    let mut out = Vec::new();
    while !src.is_empty() {
        let mut frame = FrameDecoder::new();
        match frame.reset(&mut src) {
            Ok(()) => {
                frame.decode_blocks(&mut src, BlockDecodingStrategy::All).unwrap();
                out.extend(frame.collect().unwrap_or_default());
            }
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length, ..
            })) => src = &src[length as usize..],
            Err(e) => panic!("segment {seq}: {e}"),
        }
    }
    out
}

/// How much of a run's output its log keeps is its user's to set (`shards configure disk
/// --log-history`, D186): a run made after the setting, the daemon running or not, keeps at
/// most that much of its newest output, and at least that less a segment (half of it, below
/// 40 MiB); with none set, 1 GiB, as `configure disk` says. A settings file that does not
/// read refuses a run, saying where, and `--reset` takes every setting back to its default,
/// read or not; every flag is checked before any is kept.
#[test]
fn a_logs_history_is_what_its_user_set() {
    let Some((home, image)) = home("containers-disk-logs") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let stopped = shards(&["stop", "daemon"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let shown = shards(&["configure", "disk"]);
    assert_eq!(shown.status, Some(0), "{shown}");
    assert!(
        shown.stdout.contains("log-history         1073741824 (default)"),
        "{shown}"
    );
    // Its output as bytes.
    let output = |args: &[&str]| {
        let out = common::command()
            .env("SHARDS_HOME", &*home)
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };
    // A run whose log holds `files` segments of `size` bytes at most on disk, framing and
    // all: the newest, whole but for less than a record's 13-byte head, and the one being
    // written; its newest output, as `logs` shows it.
    let logged = |name: &str, size: u64, files: u64| {
        let wrote = output(&[
            "run", "--name", name, "--pull", "never", &image, "bulk", "1000000", "7",
        ]);
        assert_eq!(wrote.len(), 1_000_000);
        let kept = output(&["logs", name]);
        assert!(
            wrote.ends_with(&kept),
            "{name}: {} bytes kept, not the newest",
            kept.len()
        );
        let dir = std::fs::read_dir(home.join("containers"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|d| {
                std::fs::read_to_string(d.join("config.json"))
                    .is_ok_and(|c| c.contains(&format!("\"name\":\"{name}\"")))
            })
            .unwrap();
        let seqs: Vec<u64> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.strip_prefix("log.")?.strip_suffix(".idx")?.parse().ok()
            })
            .collect();
        assert_eq!(seqs.len() as u64, files, "{name}: {seqs:?}");
        let written: u64 = seqs.iter().map(|seq| segment_log(&dir, *seq).len() as u64).sum();
        assert!(
            ((files - 1) * size - 13..=files * size).contains(&written),
            "{name}: {written} bytes on disk, {} of output",
            kept.len()
        );
    };
    let set = shards(&["configure", "disk", "--log-history", "64k"]);
    assert_eq!(set.status, Some(0), "{set}");
    assert!(set.stdout.contains("log-history         65536\n"), "{set}");
    logged("before", 32 << 10, 2);
    // Set again as the daemon runs: its next run keeps that.
    let set = shards(&["configure", "disk", "--log-history=128k"]);
    assert_eq!(set.status, Some(0), "{set}");
    logged("after", 64 << 10, 2);
    // A flag that does not read keeps nothing, a reset after it included.
    let refused = shards(&["configure", "disk", "--log-history", "0", "--reset"]);
    assert_ne!(refused.status, Some(0), "{refused}");
    assert!(
        refused.stderr.contains("a log keeps at least a byte"),
        "{refused}"
    );
    let shown = shards(&["configure", "disk"]);
    assert!(shown.stdout.contains("log-history         131072\n"), "{shown}");
    // A file its user broke by hand: no run, until it is reset.
    std::fs::write(home.join("disk"), "# by hand\nlog-history lots\n").unwrap();
    let broken = shards(&["run", "--rm", "--pull", "never", &image, "exit", "0"]);
    assert_ne!(broken.status, Some(0), "{broken}");
    assert!(broken.stderr.contains("disk:2: "), "{broken}");
    let reset = shards(&["configure", "disk", "--reset"]);
    assert_eq!(reset.status, Some(0), "{reset}");
    assert!(
        reset.stdout.contains("log-history         1073741824 (default)"),
        "{reset}"
    );
    let ran = shards(&["run", "--rm", "--pull", "never", &image, "exit", "0"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    let stopped = shards(&["stop", "daemon"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
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
        // Compressed once rotated (D162), on the daemon's own time: as written, or not.
        let log = segment_log(&dir, *seq).len() as u64;
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
fn a_run_is_on_the_default_bridge() {
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
    let ran = run_in(&home, &image, &["--rm", "-u", "root"], &["udp", "192.0.2.1:53"]);
    assert_eq!(ran.status, Some(1), "{ran}");
    // EHOSTUNREACH, Linux's 113 on every architecture shards runs (asm-generic/errno.h).
    assert!(
        ran.stdout.starts_with("udp error ") && ran.stdout.contains("(os error 113)"),
        "{ran}"
    );
    // At once, by the guest's clock, which a host that boots its microVM slowly leaves
    // alone: the probe waits 5 s for an answer that never comes.
    let waited: u64 = ran
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("waited "))
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(u64::MAX);
    assert!(waited < 2000, "{waited} ms: {ran}");
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

/// A run serving on the host's network: its client, and the port its server says it is
/// ready on (`ready PORT`).
fn serving(home: &Path, image: &str, command: &[&str]) -> (Child, u16) {
    let options = ["--rm", "-u", "root", "--network", "host"];
    ready_on(home, &run_args(image, &options, command))
}

/// `shards ARGS` left going, its stderr in the home: its client, and the port its server
/// says it is ready on (`ready PORT`).
fn ready_on(home: &Path, args: &[String]) -> (Child, u16) {
    let said = home.join(format!(
        "serving-{}.stderr",
        args.join("-").replace(['/', ':'], "_")
    ));
    let stderr = std::fs::File::create(&said).unwrap();
    let mut child = common::command()
        .args(args)
        .env("SHARDS_HOME", home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .unwrap();
    // Read byte by byte: what comes after the line is the test's to read.
    let mut line = Vec::new();
    let mut out = child.stdout.take().unwrap();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if std::io::Read::read(&mut out, &mut byte).unwrap() != 1 {
            panic!("{args:?} ended unready: {}", ended(&mut child, Some(&said)));
        }
        line.extend_from_slice(&byte);
    }
    child.stdout = Some(out);
    let line = String::from_utf8(line).unwrap();
    let port = line.strip_prefix("ready ").and_then(|p| p.trim().parse().ok());
    let port = port.unwrap_or_else(|| panic!("{args:?}: {line}"));
    (child, port)
}

/// A port another program holds, in its host's words, as the daemon says it (Go's, Docker
/// Desktop's on Windows: FormatMessage's text): EADDRINUSE's, WSAEADDRINUSE's.
fn in_use() -> String {
    #[cfg(unix)]
    let code = libc::EADDRINUSE;
    #[cfg(windows)]
    let code = windows_sys::Win32::Networking::WinSock::WSAEADDRINUSE;
    shards_cmdline::buildflags::os_error(&std::io::Error::from_raw_os_error(code))
}

/// What a connection to `at` is first told, and what it is sent back of `sent`.
fn talk(at: std::net::SocketAddr, sent: &str) -> String {
    use std::io::{Read as _, Write as _};
    let t = Instant::now();
    let mut c = std::net::TcpStream::connect_timeout(&at, TIMEOUT)
        .unwrap_or_else(|e| panic!("connecting to {at}: {e}, after {:?}", t.elapsed()));
    c.set_read_timeout(Some(TIMEOUT)).unwrap();
    c.write_all(sent.as_bytes()).unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = String::new();
    c.read_to_string(&mut got)
        .unwrap_or_else(|e| panic!("hearing {at}: {e}, after {:?}, {got:?} come", t.elapsed()));
    got
}

/// This host's name, as the daemon names its host's network by: gethostname(2); on Windows
/// its DNS host name (GetComputerNameExW, ComputerNamePhysicalDnsHostname).
fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut name = [0u8; 256];
        // SAFETY: gethostname(2) into a buffer of ours, of its length.
        let got = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) };
        assert_eq!(got, 0);
        std::ffi::CStr::from_bytes_until_nul(&name)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{
            ComputerNamePhysicalDnsHostname, GetComputerNameExW,
        };
        let mut name = [0u16; 256];
        let mut len = u32::try_from(name.len()).unwrap();
        // SAFETY: a buffer of ours, its length in units, which the call writes back.
        let got =
            unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, name.as_mut_ptr(), &raw mut len) };
        assert_ne!(got, 0);
        String::from_utf16_lossy(&name[..usize::try_from(len).unwrap()])
    }
}

/// `--network host` (D124, PM M177): asked for, and said each time; the microVM on the
/// host's network as dockerd's container is on its host's. The host's name; its servers
/// the host's ports, at every address and at the loopback's, TCP's and UDP's, the host
/// choosing port 0's; a port the host holds refused as Linux refuses it, and one closed
/// and bound again at once its own; the host's own services reached at its loopback, by
/// clients that bind first too; and what dockerd refuses there refused in its words.
#[test]
fn a_run_on_the_hosts_network_is_on_the_hosts_network() {
    let Some((home, image)) = home("containers-host") else {
        return;
    };
    let host = ["--rm", "-u", "root", "--network", "host"];
    let warning = format!("WARNING: {}\n", shards_abi::run::NETWORK_HOST);
    let names = run_in(
        &home,
        &image,
        &[&host[..], &["-p", "8080:80"]].concat(),
        &["report"],
    );
    assert_eq!(names.status, Some(0), "{names}");
    assert!(
        names.stderr.starts_with(&format!(
            "WARNING: Published ports are discarded when using host network mode\n{warning}"
        )),
        "{names}"
    );
    let name = host_name();
    let mut cut = name.len().min(64);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    assert!(
        names
            .stdout
            .contains(&format!("hostname {}\n", name.get(..cut).unwrap())),
        "{names}"
    );

    // The host's own services at the loopback, TCP's and UDP's, by clients that bind
    // port 0 first (as the test guest's `udp` does, and musl's resolver).
    let service = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = service.local_addr().unwrap();
    let answered = std::thread::spawn(move || {
        use std::io::Write as _;
        let (mut c, _) = service.accept().unwrap();
        c.write_all(b"hi").unwrap();
    });
    let ran = run_in(&home, &image, &host, &["tcp", &at.to_string()]);
    assert!(ran.stdout.starts_with("tcp 2 hi"), "{ran}");
    answered.join().unwrap();
    let service = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let at = service.local_addr().unwrap();
    let answered = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        let (_, peer) = service.recv_from(&mut buf).unwrap();
        service.send_to(b"!!!", peer).unwrap();
    });
    let ran = run_in(&home, &image, &host, &["udp", &at.to_string()]);
    assert_eq!(ran.stdout, "udp 3\n", "{ran}");
    answered.join().unwrap();
    // And at IPv6's loopback, `::1`, which the guest reaches by address translation,
    // where this host has it.
    if let Ok(service) = std::net::TcpListener::bind("[::1]:0") {
        let at = service.local_addr().unwrap();
        let answered = std::thread::spawn(move || {
            use std::io::Write as _;
            let (mut c, _) = service.accept().unwrap();
            c.write_all(b"six").unwrap();
        });
        let ran = run_in(&home, &image, &host, &["tcp", &at.to_string()]);
        assert!(ran.stdout.starts_with("tcp 3 six"), "{ran}");
        answered.join().unwrap();
        let service = std::net::UdpSocket::bind("[::1]:0").unwrap();
        let at = service.local_addr().unwrap();
        let answered = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let (_, peer) = service.recv_from(&mut buf).unwrap();
            service.send_to(b"!!!!!!", peer).unwrap();
        });
        let ran = run_in(&home, &image, &host, &["udp", &at.to_string()]);
        assert_eq!(ran.stdout, "udp 6\n", "{ran}");
        answered.join().unwrap();
    }

    // Its servers: at every address, reached from the gateway (eth0's own port); at the
    // loopback's, from the loopback, by a relay of the guest's, as the host's loopback
    // reaches its own; each at the port the host chose.
    let loopback = |port| std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let gateway = bridge().gateway();
    for (addr, peer) in [
        ("[::]:0", gateway.to_string()),
        ("127.0.0.1:0", "127.0.0.1".to_string()),
    ] {
        let (mut child, port) = serving(&home, &image, &["listen", addr, "1"]);
        assert_eq!(
            talk(loopback(port), "echo"),
            format!("from {peer}\necho"),
            "{addr}"
        );
        assert_eq!(exit(&mut child), Some(0), "{addr}");
    }
    if std::net::TcpListener::bind("[::1]:0").is_ok() {
        let (mut child, port) = serving(&home, &image, &["listen", "[::1]:0", "1"]);
        let at = std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
        assert_eq!(talk(at, "six"), "from ::1\nsix");
        assert_eq!(exit(&mut child), Some(0));
    }
    // A UDP server's answer, sent as its server ends: at the loopback's, by the relay;
    // at every address, from the gateway. Twelve, the race between the answer and the
    // port's letting go being the VM's to win each time.
    let answering = [
        ("127.0.0.1:0", "127.0.0.1".to_string()),
        ("0.0.0.0:0", gateway.to_string()),
    ];
    for (i, (addr, peer)) in answering.iter().cycle().take(12).enumerate() {
        let (mut child, port) = serving(&home, &image, &["udp-serve", addr]);
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(TIMEOUT)).unwrap();
        client.send_to(b"datagram", loopback(port)).unwrap();
        let mut buf = [0u8; 64];
        let n = client
            .recv(&mut buf)
            .unwrap_or_else(|e| panic!("{i} {addr}: {e}"));
        assert_eq!(
            String::from_utf8_lossy(&buf[..n]),
            format!("from {peer} datagram"),
            "{i}"
        );
        assert_eq!(exit(&mut child), Some(0));
    }

    // A port the host holds is the host's (EADDRINUSE, Linux's 98); one the guest closed
    // and binds again at once, its own, each of 50 times.
    let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let ran = run_in(&home, &image, &host, &["listen", &format!("0.0.0.0:{port}"), "1"]);
    assert_eq!(ran.status, Some(1), "{ran}");
    assert!(
        ran.stdout.starts_with("listen error ") && ran.stdout.contains("(os error 98)"),
        "{ran}"
    );
    drop(held);
    let ran = run_in(&home, &image, &host, &["rebind", "0.0.0.0:0", "50"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    assert!(ran.stdout.starts_with("rebound "), "{ran}");
    // A run's ports are the host's to bind again once its end is told, a server killed
    // with it too (PM M181), as dockerd's are.
    for i in 0..3 {
        let name = format!("ended{i}");
        let (mut child, port) = ready_on(
            &home,
            &run_args(
                &image,
                &["--name", &name, "-u", "root", "--network", "host"],
                &["listen", "0.0.0.0:0", "9"],
            ),
        );
        let killed = shards_in(&home, &["kill", &name]);
        assert_eq!(killed.status, Some(0), "{killed}");
        let _ = exit(&mut child);
        let bound = std::net::TcpListener::bind(("0.0.0.0", port));
        assert!(
            bound.is_ok(),
            "{i}: port {port} held past its run's end: {bound:?}"
        );
    }

    // What a run on its host's network is refused, in shards' words.
    for (options, said) in [
        (
            &["--sysctl", "net.ipv4.ip_forward=1"][..],
            "--sysctl net.ipv4.ip_forward: a run on its host's network has no network of its own to set it in\n",
        ),
        (
            &["--ip", "172.17.0.9"][..],
            "--network host:\n* --ip: an address of its own is a run's on a network you made\n* 172.17.0.9: in none of the network's subnets\n",
        ),
        (
            &["--network-alias", "a"][..],
            "network-scoped aliases are only supported for user-defined networks\n",
        ),
        (
            &["--network", "none"][..],
            "--network host: a run on its host's network is on no other\n",
        ),
    ] {
        let ran = run_in(&home, &image, &[&host[..], options].concat(), &["exit", "0"]);
        assert_eq!(ran.status, Some(125), "{options:?}: {ran}");
        assert!(
            ran.stderr.starts_with(&format!("shards: {said}")),
            "{options:?}: {ran}"
        );
    }
}

/// A container joining one on the host's network is on it too (D119, D124; PM M181), as
/// dockerd's is: told so, which dockerd is not; its servers the host's ports, while its
/// provider runs and after the provider has ended, a server it starts then too; the
/// provider's ports free once its end is told, the joiner's once its own is.
#[test]
fn a_joiner_of_a_container_on_the_hosts_network_is_on_it_too() {
    let Some((home, image)) = home("containers-host-join") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let args = |options: &[&str], command: &[&str]| run_args(&image, options, command);
    let (mut provider, theirs) = ready_on(
        &home,
        &args(
            &["--name", "hprov", "-u", "root", "--network", "host"],
            &["listen", "0.0.0.0:0", "9"],
        ),
    );
    let (mut joiner, ours) = ready_on(
        &home,
        &args(
            &["--name", "hjoin", "-u", "root", "--network", "container:hprov"],
            &["listen", "0.0.0.0:0", "9"],
        ),
    );
    let said = std::fs::read_dir(&*home)
        .unwrap()
        .filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
        .any(|t| t.contains(&format!("WARNING: {}", shards_abi::run::NETWORK_HOST_JOINED)));
    assert!(said, "the joiner was not told it is on the host's network");
    let loopback = |port| std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let gateway = bridge().gateway();
    assert_eq!(talk(loopback(theirs), "a"), format!("from {gateway}\na"));
    assert_eq!(talk(loopback(ours), "b"), format!("from {gateway}\nb"));
    // The provider's command ends: its port is the host's to bind again at once, while
    // the joiner's server goes on, and a server the joiner starts now is the host's too.
    let killed = shards(&["kill", "hprov"]);
    assert_eq!(killed.status, Some(0), "{killed}");
    let _ = exit(&mut provider);
    drop(std::net::TcpListener::bind(("0.0.0.0", theirs)).expect("the provider's port, free at its end"));
    assert_eq!(talk(loopback(ours), "c"), format!("from {gateway}\nc"));
    let (mut late, port) = ready_on(
        &home,
        &["exec", "hjoin", "/bin/testguest", "listen", "127.0.0.1:0", "1"].map(String::from),
    );
    assert_eq!(talk(loopback(port), "d"), "from 127.0.0.1\nd");
    assert_eq!(exit(&mut late), Some(0));
    // The joiner's end: its port the host's to bind again at once.
    let killed = shards(&["kill", "hjoin"]);
    assert_eq!(killed.status, Some(0), "{killed}");
    let _ = exit(&mut joiner);
    drop(std::net::TcpListener::bind(("0.0.0.0", ours)).expect("the joiner's port, free at its end"));
}

/// PM M181: whether a port of the host's that a run on the host's network served is the
/// host's to bind again once the run's end is told, its server killed with it: `M181_N`
/// runs alone, then as many containers joining one such run (D119), each port bound on
/// the host as soon as its client has ended, as a shell's next command would.
#[test]
#[ignore = "a measurement: docs/research/measurements/network-host/release.sh"]
fn ports_held_past_their_containers_end() {
    let Some((home, image)) = home("containers-m181") else {
        return;
    };
    let n: usize = std::env::var("M181_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let loopback = |port| std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let serve = |name: &str, network: &str| {
        ready_on(
            &home,
            &run_args(
                &image,
                &["--name", name, "-u", "root", "--network", network],
                &["listen", "0.0.0.0:0", "9"],
            ),
        )
    };
    let killed = |child: &mut Child, name: &str| {
        let killed = shards_in(&home, &["kill", name]);
        assert_eq!(killed.status, Some(0), "{killed}");
        // As a shell waits: at once, not polled.
        let _ = child.wait();
    };
    let (mut runs, mut joiners) = (0, 0);
    for i in 0..n {
        let name = format!("a{i}");
        let (mut run, port) = serve(&name, "host");
        let _ = talk(loopback(port), "x");
        killed(&mut run, &name);
        runs += usize::from(std::net::TcpListener::bind(("0.0.0.0", port)).is_err());
    }
    for i in 0..n {
        let (p, j) = (format!("p{i}"), format!("j{i}"));
        let (mut provider, _) = serve(&p, "host");
        let (mut joiner, port) = serve(&j, &format!("container:{p}"));
        let _ = talk(loopback(port), "x");
        killed(&mut joiner, &j);
        joiners += usize::from(std::net::TcpListener::bind(("0.0.0.0", port)).is_err());
        killed(&mut provider, &p);
    }
    println!("runs: held {runs} of {n}");
    println!("joiners: held {joiners} of {n}");
}

/// An address a run asked for on its network is free once the run's end is told, as
/// dockerd's is once `run --rm` returns (PM M182): a run asking for it at once after, each
/// of 10 times, the last one's end not yet taken.
#[test]
fn an_asked_address_is_free_once_its_run_has_ended() {
    let Some((home, image)) = home("containers-ip-reuse") else {
        return;
    };
    let made = shards_in(&home, &["network", "create", "--subnet", "10.78.2.0/24", "reuse"]);
    assert_eq!(made.status, Some(0), "{made}");
    for i in 0..10 {
        let ran = run_in(
            &home,
            &image,
            &["--rm", "--network", "reuse", "--ip", "10.78.2.9"],
            &["exit", "0"],
        );
        assert_eq!(ran.status, Some(0), "{i}: {ran}");
    }
}

/// PM M182: what a command's datagrams to a member of its network come to as it ends: one,
/// to a member the sender's guest never sent to (so asked for by ARP), the command ending
/// at once, or after 500 ms; a burst of 1,400-byte datagrams, ending at once, or after a
/// second. `M182_N` of each, interleaved; the member's count of what came.
#[test]
#[ignore = "a measurement: docs/research/measurements/exit-drain/run.sh"]
fn datagrams_a_command_sends_as_it_ends() {
    let Some((home, image)) = home("containers-m182") else {
        return;
    };
    let n: usize = std::env::var("M182_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let burst: usize = std::env::var("M182_BURST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let made = shards_in(&home, &["network", "create", "--subnet", "10.78.1.0/24", "drain"]);
    assert_eq!(made.status, Some(0), "{made}");
    let cases = [
        ("one, ending at once", 1, 64, 0),
        ("one, ending after 500 ms", 1, 64, 500),
        ("a burst, ending at once", burst, 1400, 0),
        ("a burst, ending after 1 s", burst, 1400, 1000),
    ];
    let mut got: Vec<Vec<u64>> = vec![Vec::new(); cases.len()];
    for i in 0..n * cases.len() {
        let c = i % cases.len();
        let (_, count, size, linger) = cases[c];
        let (mut counter, port) = ready_on(
            &home,
            &run_args(
                &image,
                &["--rm", "--network", "drain", "--ip", "10.78.1.50"],
                &["udp-count", "0.0.0.0:0", "1000"],
            ),
        );
        let sent = run_in(
            &home,
            &image,
            &["--rm", "--network", "drain"],
            &[
                "udp-send",
                &format!("10.78.1.50:{port}"),
                &count.to_string(),
                &size.to_string(),
                &linger.to_string(),
            ],
        );
        assert_eq!(sent.status, Some(0), "{sent}");
        let mut rest = String::new();
        std::io::Read::read_to_string(counter.stdout.as_mut().unwrap(), &mut rest).unwrap();
        let _ = counter.wait();
        let n: u64 = rest
            .lines()
            .find_map(|l| l.strip_prefix("got "))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{rest}"));
        if std::env::var_os("M182_COUNTERS").is_some() {
            println!(
                "{} sender: {}",
                cases[c].0,
                sent.stdout.lines().last().unwrap_or_default()
            );
            println!(
                "{} receiver: {}",
                cases[c].0,
                rest.lines().last().unwrap_or_default()
            );
        }
        got[c].push(n);
    }
    for ((name, count, _, _), got) in cases.iter().zip(&mut got) {
        got.sort_unstable();
        let short = got.iter().filter(|&&g| g < *count as u64).count();
        println!(
            "{name}: sent {count}, short in {short} of {n}; got min {} p50 {} max {}",
            got.first().unwrap(),
            got[got.len() / 2],
            got.last().unwrap()
        );
    }
}

/// A run's gateway is its neighbor from its start, its MAC the one its network process
/// answers ARP and neighbor solicitations with, for good (PM M180): on the default bridge,
/// past a MAC of its own (which flushes eth0's neighbors), on a network of its own, IPv6's
/// too, the template's gone, and on the host's network. A datagram the guest held for
/// the answer was lost as the run's ports went.
#[test]
fn a_runs_gateway_is_its_neighbor_from_its_start() {
    let Some((home, image)) = home("containers-gateway-mac") else {
        return;
    };
    // Its unicast neighbors: a multicast group's (`noarp`), which the kernel makes as it
    // sends MLD's reports on a network with IPv6, asks no one.
    let neighbors = |options: &[&str]| -> Vec<String> {
        let r = run_in(&home, &image, &[&["--rm"][..], options].concat(), &["neighbors"]);
        assert_eq!(r.status, Some(0), "{options:?}: {r}");
        r.stdout
            .lines()
            .filter_map(|l| l.strip_prefix("neighbor "))
            .filter(|n| !n.ends_with(" noarp"))
            .map(String::from)
            .collect()
    };
    let mac = |gateway: std::net::Ipv4Addr| {
        let [a, b, c, d] = gateway.octets();
        format!("02:42:{a:02x}:{b:02x}:{c:02x}:{d:02x}")
    };
    let gateway = bridge().gateway();
    let on_bridge = format!("{gateway} {} permanent", mac(gateway));
    for options in [&[][..], &["--mac-address", "02:42:ac:11:00:99"][..]] {
        assert_eq!(
            neighbors(options),
            std::slice::from_ref(&on_bridge),
            "{options:?}"
        );
    }
    let made = shards_in(
        &home,
        &[
            "network",
            "create",
            "--subnet",
            "10.79.0.0/24",
            "--ipv6",
            "--subnet",
            "fd7e::/64",
            "gwnet",
        ],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let own = mac(std::net::Ipv4Addr::new(10, 79, 0, 1));
    let mut got = neighbors(&["--network", "gwnet"]);
    got.sort();
    assert_eq!(
        got,
        [
            format!("10.79.0.1 {own} permanent"),
            format!("fd7e::1 {own} permanent")
        ]
    );
    let got = neighbors(&["-u", "root", "--network", "host"]);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.contains(&on_bridge), "{got:?}");
    assert!(
        got.iter().any(|n| n
            .strip_suffix(&format!(" {} permanent", mac(gateway)))
            .and_then(|a| a.parse::<std::net::Ipv6Addr>().ok())
            .is_some_and(|a| a.segments()[0] & 0xff00 == 0xfd00 && a.segments()[7] == 1)),
        "{got:?}"
    );
}

/// PM M247: how long a member's lookup of a name its network holds takes to be answered,
/// which a resolver that caches pays each time now that the network's answers are kept by
/// no cache (D46): `dnsrtt` asks 127.0.0.11, which init relays to the network process,
/// `M247_N` times a millisecond apart, from inside the microVM.
#[test]
#[ignore = "a measurement: docs/research/measurements/dns-lookup/run.sh"]
fn a_names_lookup_round_trip() {
    let Some((home, image)) = home("containers-m247") else {
        return;
    };
    let n: usize = std::env::var("M247_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let shards = |args: &[&str]| shards_in(&home, args);
    let made = shards(&["network", "create", "look"]);
    assert_eq!(made.status, Some(0), "{made}");
    let mut srv = start(&home, &image, &["--name", "srv", "--network", "look"], &["sleep"]);
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "look"],
        &["dnsrtt", "srv", &n.to_string()],
    );
    assert!(r.stdout.starts_with("dnsrtt n "), "{r}");
    println!("{}", r.stdout.trim());
    assert_eq!(shards(&["rm", "-f", "srv"]).status, Some(0));
    let _ = srv.kill();
    let _ = srv.wait();
}

/// PM M180: a UDP server on the host's network answers one datagram from this host and
/// exits, at the loopback's address (its answer relayed by init's supervisor) and at
/// every address (its own); each run interleaved with one whose guest first deleted its
/// neighbors, as a guest's gateway was unknown before init gave its MAC, so that its
/// answer waits on ARP's. How many answers were lost (none in 5 s), of `M180_N` each.
#[test]
#[ignore = "a measurement: docs/research/measurements/gateway-neighbor/run.sh"]
fn answers_lost_without_the_gateways_mac() {
    let Some((home, image)) = home("containers-m180") else {
        return;
    };
    let n: usize = std::env::var("M180_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let cases = [
        ("relay known", "127.0.0.1:0", false),
        ("relay forgotten", "127.0.0.1:0", true),
        ("direct known", "0.0.0.0:0", false),
        ("direct forgotten", "0.0.0.0:0", true),
    ];
    let mut lost = [0usize; 4];
    for i in 0..n * cases.len() {
        let (_, addr, forget) = cases[i % cases.len()];
        let options = [
            "--rm",
            "-u",
            "root",
            "--cap-add",
            "NET_ADMIN",
            "--network",
            "host",
        ];
        let mut command = vec!["udp-serve", addr];
        if forget {
            command.push("forget");
        }
        let mut child = common::command()
            .args(run_args(&image, &options, &command))
            .env("SHARDS_HOME", &*home)
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = Vec::new();
        let mut out = child.stdout.take().unwrap();
        let mut byte = [0u8; 1];
        while line.last() != Some(&b'\n') {
            if std::io::Read::read(&mut out, &mut byte).unwrap() != 1 {
                panic!("{command:?} ended unready: {}", ended(&mut child, None));
            }
            line.extend_from_slice(&byte);
        }
        child.stdout = Some(out);
        let line = String::from_utf8(line).unwrap();
        let port: u16 = line
            .strip_prefix("ready ")
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("{command:?}: {line}"));
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.send_to(b"datagram", ("127.0.0.1", port)).unwrap();
        let mut buf = [0u8; 64];
        if client.recv(&mut buf).is_err() {
            lost[i % cases.len()] += 1;
        }
        assert_eq!(exit(&mut child), Some(0), "{command:?}");
    }
    for ((name, _, _), lost) in cases.iter().zip(lost) {
        println!("{name}: lost {lost} of {n}");
    }
}

/// What dockerd says of networks a run cannot have, and when: a network that does not
/// exist fails the start, the container left created with 128 (`--rm` takes it); what
/// dockerd refuses outright makes no container.
#[test]
fn a_run_on_a_network_it_cannot_have_says_why() {
    let Some((home, image)) = home("containers-badnet") else {
        return;
    };
    let help = "\n\nRun 'shards run --help' for more information\n";
    let missing = run_in(&home, &image, &["--name", "lost", "--network", "foo"], &["true"]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (
            Some(125),
            format!("shards: --network foo: no network is named foo{help}").as_str()
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
            .contains("--network none: a run with no network is on no other"),
        "{removed}"
    );
    // An address on the bridge the daemon elected, which only a user-defined network takes.
    let on_bridge = format!(
        "name=bridge,ip={}",
        std::net::Ipv4Addr::from(u32::from(bridge().subnet().0) + 9)
    );
    for (options, said) in [
        (
            &["--network", on_bridge.as_str()][..],
            "shards: --network bridge: --ip: an address of its own is a run's on a network you made",
        ),
        (
            &["--network", "container:x", "--hostname", "h"][..],
            "shards: --hostname: a run joining another's microVM takes that run's hostname",
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
/// (moby container.StopSignal): here SIGUSR1, which ends the command 128 + 10 under
/// --init, where it is not PID 1, which hears only what it handles (Docker 29.3.1 the
/// same, M145). One no signal's name is refused as dockerd refuses it, before a container
/// is made.
#[test]
fn stop_sends_a_containers_own_stop_signal() {
    let Some((home, image)) = home("containers-stopsignal") else {
        return;
    };
    let mut run = start(
        &home,
        &image,
        &["--name", "usr1", "--stop-signal", "SIGUSR1", "--init"],
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
            "shards: BOGUS is no signal: give its name (TERM, SIGKILL, RTMIN+3) or its number\n\nRun 'shards run --help' for more information\n"
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
fn exec_runs_commands_in_a_running_microvm() {
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
    // -d: answered once it starts, its client's output ended then while the command runs
    // on, as `docker exec -d` holds nothing of its client's (before, it was held to the
    // command's end, and this read waited out its 60 s).
    let detached = Instant::now();
    assert_eq!(exec(&["-d", "ex", "/bin/testguest", "sleep"]).status, Some(0));
    assert!(
        detached.elapsed() < Duration::from_secs(10),
        "{:?}",
        detached.elapsed()
    );
    let refused = |args: &[&str], status: i32, said: &str| {
        let r = exec(args);
        assert_eq!(
            (r.status, r.stderr.as_str()),
            (Some(status), said),
            "{args:?}\n{r}"
        );
    };
    let oci = "shards: cannot start the command:";
    refused(
        &["ex", "nonexistent"],
        127,
        &format!("{oci} exec: \"nonexistent\": executable file not found in $PATH\n"),
    );
    refused(
        &["-w", "/nonexistent", "ex", "/bin/testguest", "report"],
        127,
        &format!("{oci} chdir to cwd (\"/nonexistent\") failed: no such file or directory\n"),
    );
    refused(
        &["-w", "rel", "ex", "/bin/testguest", "report"],
        128,
        &format!("{oci} Cwd must be an absolute path\n"),
    );
    refused(
        &["nope", "/bin/testguest", "report"],
        1,
        "shards: No such container: nope\n",
    );
    let unknown = exec(&["-u", "nobodyhere", "ex", "/bin/testguest", "report"]);
    assert_eq!(unknown.status, Some(1), "{unknown}");
    assert!(
        unknown
            .stderr
            .starts_with("shards: unable to find user nobodyhere"),
        "{unknown}"
    );
    // -it with a stdin that is no terminal: refused once the container is found, and not
    // with -d; the detach keys are checked before it is looked for (docker/cli exec.go).
    let untty = "-it: stdin is not a terminal, so it cannot be the command's: leave out -t, or run it from a terminal\n";
    refused(&["-it", "ex", "/bin/testguest", "report"], 1, untty);
    refused(
        &["-it", "nope", "/bin/testguest", "report"],
        1,
        "shards: No such container: nope\n",
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
    refused(
        &["ex", "/bin/testguest", "report"],
        1,
        "shards: ex is not running\n",
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
fn published_ports_reach_the_guest() {
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
    // The writer holds the connection's own socket, the reader a copy: on Windows a
    // shutdown through a copy (try_clone, WSADuplicateSocketW) of a connection to a
    // network process's port was refused, WSAENOTCONN, its FIN never sent (jupiter, 3 of
    // 3 runs), where the socket's own sent it (3 of 3).
    let exchange = |at: SocketAddr, payload: Vec<u8>| -> (String, Vec<u8>) {
        let mut writer = TcpStream::connect_timeout(&at, TIMEOUT).unwrap();
        let mut c = writer.try_clone().unwrap();
        c.set_read_timeout(Some(TIMEOUT)).unwrap();
        let len = payload.len();
        let t = Instant::now();
        let writing = std::thread::spawn(move || {
            let wrote = writer
                .write_all(&payload)
                .map_err(|e| format!("writing: {e} after {:?}", t.elapsed()));
            let shut = writer
                .shutdown(std::net::Shutdown::Write)
                .map_err(|e| format!("shutting: {e} after {:?}", t.elapsed()));
            (wrote, shut)
        });
        let mut got = Vec::new();
        let read = c.read_to_end(&mut got);
        let wrote = writing.join().unwrap();
        if read.is_err() || wrote.0.is_err() || wrote.1.is_err() {
            panic!(
                "exchanging {len} bytes with {at}: read {} bytes, {read:?}, after {:?}; writer {wrote:?}",
                got.len(),
                t.elapsed()
            );
        }
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
    // Taken by a running microVM: refused with the words scripts look for, the run left
    // created.
    let help = "\n\nRun 'shards run --help' for more information\n";
    let late = run_in(
        &home,
        &image,
        &["--name", "late", "-p", &format!("{n}:7000")],
        &["exit", "0"],
    );
    assert_eq!(
        (late.status, late.stderr.as_str()),
        (
            Some(125),
            format!(
                "shards: -p: publishing late's ports: 0.0.0.0:{n}: port is already allocated, to another run{help}"
            )
            .as_str()
        ),
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
            "127.0.0.1:{n}: port is already allocated, to another run"
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
            "0.0.0.0:{h}/tcp: the host's port is taken: {}{help}",
            in_use()
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
        // Bound by a program of its own, as the next program to bind it would: a socket
        // this process made would stay open in whatever child another test spawned
        // meanwhile, and hold p from the next run (common::port_free).
        assert!(
            common::port_free(p),
            "{p} after a run ended: taken; {}",
            catch_holder(p)
        );
        freed = Instant::now();
    }
    // And the daemon holds it no longer: taken by another program, it is refused at once,
    // not after the wait for a run's ports to come free.
    let held = common::hold_port(p);
    let began = Instant::now();
    let refused = run_in(
        &home,
        &image,
        &["--rm", "-p", &format!("{p}:7000")],
        &["exit", "0"],
    );
    let took = began.elapsed();
    assert!(
        refused.stderr.contains(&format!(
            "0.0.0.0:{p}/tcp: the host's port is taken: {}",
            in_use()
        )),
        "{refused}"
    );
    // The daemon's word on any wait for a run's ports, which its log keeps: none, each run's
    // ports freed as it said its network process had closed them (PM M380), where the wait
    // for that process to go took past 2 s on CI's Windows runners.
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let waits: Vec<&str> = log.lines().filter(|l| l.contains(&format!(":{p}: "))).collect();
    eprintln!("refused in {took:?}");
    assert!(waits.is_empty(), "the daemon's log of {p}: {waits:#?}");
    drop(held);
}

/// A run's published port is free once `run` returns while the daemon makes VMs all the
/// while, for three others running at once: every child it makes is its spawner's, which
/// holds none of the listeners the daemon binds, where a child the daemon made itself
/// would hold every one it had until that child's exec (PM M134, M158).
#[test]
fn published_ports_are_free_at_once_while_the_daemon_makes_vms() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    /// The other runs stop however this test's own ends, a panic's unwinding included:
    /// the scope waits for them.
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let Some((home, image)) = home("containers-publish-spawning") else {
        return;
    };
    let p = fixed_port();
    let (stop, others) = (AtomicBool::new(false), AtomicU32::new(0));
    let other_failed = Mutex::new(None);
    let failed = std::thread::scope(|s| {
        let _stop = Stop(&stop);
        for _ in 0..3 {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let other = run_in(&home, &image, &["--rm"], &["exit", "0"]);
                    if other.status != Some(0) {
                        *other_failed.lock().unwrap() = Some(other.to_string());
                        return;
                    }
                    others.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        (0..30).find_map(|i| {
            let run = run_in(
                &home,
                &image,
                &["--rm", "-p", &format!("{p}:7000")],
                &["exit", "0"],
            );
            if run.status != Some(0) {
                return Some(format!("run {i} of 30: {run}"));
            }
            // Bound by a program of its own, which makes no children (common::port_free).
            (!common::port_free(p))
                .then(|| format!("{p} after run {i} of 30 ended: taken; {}", catch_holder(p)))
        })
    });
    let others = others.into_inner();
    assert_eq!(failed, None, "with {others} other runs meanwhile");
    assert_eq!(other_failed.into_inner().unwrap(), None, "another run");
    assert!(others >= 3, "{others} other runs: too few to tell");
    // Whatever the timing: a running VM is its spawner's child, not the daemon's, whose
    // one child is the spawner; on Windows, where a process is made, not forked, there is
    // no spawner and a VM is the daemon's own (D158, D195).
    let mut asleep = start(&home, &image, &["--name", "asleep"], &["sleep"]);
    let daemon: u32 = std::fs::read_to_string(home.join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let children = children_of(daemon);
    let spawner = if cfg!(windows) {
        daemon
    } else {
        match children.as_slice() {
            [(pid, args)] if args.ends_with(" spawner") => *pid,
            _ => panic!("the daemon's children: {children:?}"),
        }
    };
    let made = children_of(spawner);
    assert!(
        made.iter().any(|(_, args)| args.contains("shards-vm")),
        "the spawner's children: {made:?}"
    );
    assert_eq!(shards_in(&home, &["stop", "asleep"]).status, Some(0));
    exit(&mut asleep);
}

/// The processes whose parent is `parent`, each with its command line: from /proc on
/// Linux, whatever ps the host has, elsewhere as ps lists them; on Windows each with its
/// executable's name alone, from the system's snapshot of its processes (Toolhelp32).
fn children_of(parent: u32) -> Vec<(u32, String)> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| {
                let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                // After the command's name, in parentheses: its state, then its parent.
                let ppid: u32 = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
                let args = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                let args = String::from_utf8_lossy(&args)
                    .trim_end_matches('\0')
                    .replace('\0', " ");
                (ppid == parent).then_some((pid, args))
            })
            .collect()
    }
    #[cfg(windows)]
    {
        common::processes()
            .into_iter()
            .filter(|(_, ppid, _)| *ppid == parent)
            .map(|(pid, _, name)| (pid, name))
            .collect()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let out = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid: u32 = fields.next()?.parse().ok()?;
                let ppid: u32 = fields.next()?.parse().ok()?;
                (ppid == parent).then(|| (pid, fields.collect::<Vec<_>>().join(" ")))
            })
            .collect()
    }
}

/// `-p PORT/udp`: datagrams reach the guest as dockerd's proxy carries them: each host
/// peer from a gateway port of its own, its answers back to that peer from the address
/// it asked; `port` and `ps` list the port as UDP. The guest answers until it is sent
/// `bye`: the published port is one of the host's ephemeral ports, which a datagram meant
/// for an earlier holder of the number may still reach.
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
        &["udp-echo", "5353", "bye"],
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
    // An answer that never comes says what each side saw: the guest's datagrams (its
    // output), whether the run and its processes live, the daemon's log, and the host's
    // UDP sockets on the port (a lost answer, 2026-10-06 and 2026-10-10, under load).
    let ask = |client: &UdpSocket, to: std::net::SocketAddr, payload: &[u8]| {
        client.set_read_timeout(Some(TIMEOUT)).unwrap();
        client
            .send_to(payload, to)
            .unwrap_or_else(|e| panic!("sending to {to} from {:?}: {e}", client.local_addr()));
        let mut buf = [0u8; 2048];
        let (len, from) = common::recv_from(client, &mut buf).unwrap_or_else(|e| {
            let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
            let logs = shards_in(&home, &["logs", "dns"]);
            let listed = shards_in(&home, &["ps", "-a"]);
            let state = shards_in(&home, &["inspect", "-f", "{{json .State}}", "dns"]);
            let sockets = std::process::Command::new("netstat")
                .args(["-an", "-p", "udp"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let port = to.port().to_string();
            let ours: Vec<&str> = sockets.lines().filter(|l| l.contains(&port)).collect();
            panic!(
                "no answer to {payload:?} from {to}: {e}\n--- logs\n{logs}\n--- ps -a\n{listed}\n--- State\n{state}\n--- the daemon's processes\n{}\n--- netstat {port}\n{}\n--- daemon.log\n{log}",
                common::processes_under(&log),
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
    // address asked, not the one the route back would pick. Windows sends nothing from its
    // loopback to its other addresses (WSAENETUNREACH, jupiter 2026-10-28): there the ask
    // comes from a socket of the unspecified address.
    let host = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    let any = UdpSocket::bind("0.0.0.0:0").unwrap();
    let asker = if cfg!(windows) { &any } else { &a };
    match host {
        Ok(host) => {
            let at = std::net::SocketAddr::new(host.ip(), n);
            let (answer, from) = ask(asker, at, b"one");
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
    let (bye, _) = ask(&a, v4, b"bye");
    gateway_port(&bye, "bye");
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
fn images_lists_what_was_pulled() {
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
        format!("shards: failed to resolve reference \"{missing}\": {missing}: not found\n"),
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
fn tag_names_an_image_again() {
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
        (&["nosuch:1", "y"][..], "shards: No such image: nosuch:1"),
        (&["dead", "y"], "shards: No such image: dead:latest"),
        (
            &[image.as_str(), "Bad:1"],
            "error parsing reference: \"Bad:1\" is not a valid repository/tag: invalid reference format: repository name (library/Bad) must be lowercase",
        ),
        (
            &[image.as_str(), &digest],
            "shards: a tag names an image by name and tag, not by digest",
        ),
        (
            &[image.as_str(), "sha256"],
            "shards: an image named sha256 would read as a digest: give it another name",
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
fn rmi_removes_images() {
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
            "shards: No such image: shp/nosuch:1\n"
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
                "shards: cannot remove image {image}: microVM {container} was made of it, {id}: -f removes it anyway\n"
            )
        ),
        "{used}"
    );
    let hard = shards(&["rmi", "-f", &id]);
    assert_eq!(
        hard.stderr,
        format!(
            "shards: cannot remove image {id}: microVM {container} runs it, which -f does not take from it\n"
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
            "microVM {container} was made of it: -f removes it anyway"
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
fn image_inspect_describes_images() {
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
    assert_eq!(shown.stderr, "shards: No such image: nosuch:1\n");
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
    // Named short: a socket is bound in it by its whole path (sockaddr_un).
    let home = TempDir::new("save-int");
    // A daemon that never answers: on Unix one of an older build listening at daemon.sock;
    // on Windows the home's lock held, which the daemon its client starts waits for.
    #[cfg(unix)]
    let _daemon = std::os::unix::net::UnixListener::bind(home.join("daemon.sock")).unwrap();
    #[cfg(windows)]
    let lock = {
        let lock = std::fs::File::create(home.join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        lock
    };
    let out = TempDir::new("containers-save-interrupted-out");
    let dest = out.join("image.tar");
    let left = || -> Vec<std::ffi::OsString> {
        std::fs::read_dir(&*out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    };
    // Windows sends a console process one event, its interrupt (common::interrupt).
    #[cfg(unix)]
    let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
    #[cfg(windows)]
    let signals = [2];
    for sig in signals {
        let mut save = common::command();
        save.args(["save", "-o", dest.to_str().unwrap(), "any:1"])
            .env("SHARDS_HOME", &*home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: signal(2), async-signal-safe, between fork and exec.
        #[cfg(unix)]
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut save, || {
                for s in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
                    libc::signal(s, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let mut save = common::interruptible(&mut save).spawn().unwrap();
        eventually("the save's temporary file", || !left().is_empty());
        #[cfg(unix)]
        {
            // SAFETY: kill(2) of the client this test started.
            assert_eq!(unsafe { libc::kill(save.id() as libc::pid_t, sig) }, 0);
            let status = save.wait().unwrap();
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&status),
                Some(sig),
                "{status}"
            );
        }
        #[cfg(windows)]
        {
            common::interrupt(&save);
            exit(&mut save);
        }
        assert_eq!(left(), Vec::<std::ffi::OsString>::new(), "after signal {sig}");
    }
    // The daemon its client started, which waits for the lock, ended: named by its log's
    // lines (`shards daemon PID: ...`).
    #[cfg(windows)]
    {
        let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
        if let Some(pid) = log.lines().find_map(|l| {
            l.strip_prefix("shards daemon ")?
                .split_once(':')?
                .0
                .parse::<i32>()
                .ok()
        }) {
            common::kill_now(pid);
        }
        drop(lock);
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
fn save_writes_images() {
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
    // Its mode, 0600 whatever the umask, as atomicwriter's Close chmods it: made under
    // umask 0377, the file would be 0400. A Windows file has no mode, nor its process a
    // umask: its owner's alone there is crate::os's `owner_only`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&tar).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
    }
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
        (Some(1), "shards: No such image: nosuch:1\n"),
        "{refused}"
    );
    assert!(!missing.exists());
    assert_eq!(std::fs::read_dir(&*out).unwrap().count(), 1, "only image.tar");
    // In the host's own words, as Go's os.Stat says them there (os/stat_windows.go).
    let bad = shards(&["save", "-o", "/nonexistent-dir/x.tar", &image]);
    let said = if cfg!(windows) {
        "failed to save image: invalid output path: GetFileAttributesEx \\nonexistent-dir: The system cannot find the file specified.\n"
    } else {
        "failed to save image: invalid output path: stat /nonexistent-dir: no such file or directory\n"
    };
    assert_eq!((bad.status, bad.stderr.as_str()), (Some(1), said));
    let dir = shards(&["save", "-o", out.to_str().unwrap(), &image]);
    assert_eq!(dir.stderr, "failed to save image: cannot write to a directory\n");
    // A blob that cannot be read past the archive's start: the save fails, and its file
    // is neither made nor left half written beside.
    let layer = home
        .join("images/blobs/sha256")
        .join(sha256_digest(&blobs[1]).trim_start_matches("sha256:"));
    // Unreadable: no mode on Unix; on Windows held open by this test, shared with no one.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&layer, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    #[cfg(windows)]
    let held = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&layer)
            .unwrap()
    };
    let broken = out.join("broken.tar");
    let failed = shards(&["save", "-o", broken.to_str().unwrap(), &image]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&layer, std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    #[cfg(windows)]
    drop(held);
    assert_eq!(failed.status, Some(1), "{failed}");
    assert!(failed.stderr.starts_with("shards: "), "{failed}");
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
        (Some(1), "shards: No such image: nosuchrepo:latest\n"),
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
fn load_reads_archives() {
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
    // a run of it runs, under its CPU's emulator (D179; its program here this host's
    // own), warned of as dockerd warns of it.
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
    assert_eq!(ran.status, Some(0), "{ran}");
    assert!(
        ran.stderr.contains(
            "WARNING: The requested image's platform (linux/s390x) does not match the detected host platform"
        ),
        "{ran}"
    );
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
    // In the host's own words, as Go's open says them there.
    let missing = shards(&["load", "-i", "/nonexistent.tar"]);
    let said = if cfg!(windows) {
        "open /nonexistent.tar: The system cannot find the file specified.\n"
    } else {
        "open /nonexistent.tar: no such file or directory\n"
    };
    assert_eq!(missing.stderr, said);
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
fn a_run_interrupted_as_it_pulls_ends_at_once() {
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
    #[cfg(unix)]
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut client, || {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    let client = common::interruptible(&mut client)
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
    common::interrupt(&client);
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
fn push_uploads_images() {
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

/// A guest holds open as many files of a shared directory as its own kernel lets it (audit
/// V09): the share's descriptors are a cache it lets go and opens again by path, never a
/// limit the guest meets. 300 files of a `-v` directory, past what this host lets a share
/// hold, are open at once and read round and round.
#[test]
fn a_shared_directorys_files_stay_open_past_the_shares_descriptors() {
    let Some((home, image)) = home("containers-many-open") else {
        return;
    };
    let shared = home.join("many");
    std::fs::create_dir(&shared).unwrap();
    for i in 0..300 {
        std::fs::write(shared.join(format!("f{i}")), vec![b'x'; 4096]).unwrap();
    }
    let bind = format!("{}:/data:ro", shared.display());
    let ran = run_in(
        &home,
        &image,
        &["--rm", "-u", "0", "-v", &bind],
        &["fs", "roundrobin:300:/data:2"],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    assert!(ran.stdout.contains("roundrobin n 600 "), "{ran}");
}

/// `-v` and `--mount`, as `docker run` takes them (D38): a host directory shared both ways
/// and owned as Docker Desktop records it, read-only where asked, a file bound alone; a
/// named volume filled from the image where it is empty, and kept; tmpfs; inspect's
/// mounts; anonymous volumes removed with `--rm` and `rm -v`, and kept by `rm`.
#[test]
fn run_mounts_binds_volumes_and_tmpfs() {
    let Some((home, image)) = home("containers-volumes") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let shared = home.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("a"), "host").unwrap();
    let file = home.join("one");
    std::fs::write(&file, "alone").unwrap();
    // A link to it, bound as the file it leads to on the host, as Docker's bind takes it.
    let link = home.join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&file, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&file, &link).unwrap();
    let bind = format!("{}:/data", shared.display());
    let ro = format!("{}:/ro:ro", shared.display());
    let single = format!("{}:/etc/one:ro", file.display());
    let linked = format!("{}:/etc/linked:ro", link.display());
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
            "-v",
            &linked,
        ],
        &["stat", "/data/a", "/etc/one", "/etc/linked", "/proc/self/mounts"],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let host = host_file_mode();
    for (path, said) in [
        ("/data/a", "4\n= host"),
        ("/etc/one", "5\n= alone"),
        ("/etc/linked", "5\n= alone"),
    ] {
        assert!(
            ran.stdout.contains(&format!("{path} file {host} 0:0 {said}\n")),
            "{path}: {ran}"
        );
    }
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
    // The binds as JSON has them: a Windows path's backslashes escaped.
    assert_eq!(
        shown.stdout,
        format!(
            "volume /anon true  ;bind /data true  rprivate;volume /v true z ; {} {{\"/anon\":{{}}}}\n",
            serde_json::to_string(&[&bind]).unwrap()
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
/// A fresh volume where its image has nothing at the mount point is its run's user's (D38,
/// PM M249): its top owned by the run's user and group, so that a run as other than root
/// writes the volume it was given, as root's top would not let it; a root run's is root's;
/// the first run's user keeps it for the runs after; and one where the image has a
/// directory takes that directory's owner, as it did.
#[test]
fn a_fresh_volume_where_its_image_has_nothing_is_its_runs_users() {
    let Some((home, image)) = home("containers-fresh") else {
        return;
    };
    // The image runs as app, 1000:1000, and has no /data.
    let wrote = run_in(
        &home,
        &image,
        &["--rm", "-v", "fresh:/data"],
        &["fs", "write:/data/x=1"],
    );
    assert_eq!(wrote.status, Some(0), "{wrote}");
    let seen = run_in(&home, &image, &["--rm", "-v", "fresh:/data"], &["stat", "/data"]);
    assert!(seen.stdout.starts_with("/data dir 755 1000:1000 "), "{seen}");
    let rooted = run_in(
        &home,
        &image,
        &["--rm", "-u", "0", "-v", "rooted:/data"],
        &["stat", "/data"],
    );
    assert!(rooted.stdout.starts_with("/data dir 755 0:0 "), "{rooted}");
    let other = run_in(
        &home,
        &image,
        &["--rm", "-u", "2000:2000", "-v", "fresh:/data"],
        &["fs", "write:/data/y=2"],
    );
    assert_ne!(other.status, Some(0), "{other}");
    let imaged = run_in(
        &home,
        &image,
        &["--rm", "-u", "0", "-v", "homes:/home/app"],
        &["stat", "/home/app"],
    );
    assert!(
        imaged.stdout.starts_with("/home/app dir 755 1000:1000 "),
        "{imaged}"
    );
}

#[test]
fn volume_commands_keep_volumes() {
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
        (Some(1), "shards: create other: invalid option: \"bad\"\n")
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
            format!("shards: cannot remove volume data: in use by {id}\n")
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
        (Some(1), "[]\n", "shards: no volume is named nope\n")
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
#[test]
fn restart_policies_start_microvms_again() {
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

/// A run refused a process or thread past its `--pids-limit` says so (D193): a
/// `pids-limit` event of its container's, with the limit and how many it refused, which no
/// state of Docker's has, and the daemon's log of what its cgroup was allowed and held; a
/// run within its limit says nothing of it; and a container joined to another's network
/// (D119), held to its own limit in its own cgroup, says so of its own.
#[test]
fn a_run_past_its_pids_limit_says_so() {
    let Some((home, image)) = home("containers-pids") else {
        return;
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let since = now().to_string();
    // Eight threads at once where four tasks are allowed, its own among them.
    let past = run_in(
        &home,
        &image,
        &["--rm", "--name", "forked", "--pids-limit", "4"],
        &["burn", "8", "100"],
    );
    assert!(past.stdout.contains("of 8 threads"), "{past}");
    let within = run_in(
        &home,
        &image,
        &["--rm", "--name", "within", "--pids-limit", "64"],
        &["burn", "2", "100"],
    );
    assert!(within.stdout.contains("burned"), "{within}");
    let until = (now() + 1).to_string();
    let events = shards_in(
        &home,
        &[
            "events",
            "--since",
            &since,
            "--until",
            &until,
            "--filter",
            "event=pids-limit",
            "--format",
            r#"{{.Actor.Attributes.name}} {{index .Actor.Attributes "pids.max"}} {{.Actor.Attributes.refused}}"#,
        ],
    );
    assert_eq!(events.status, Some(0), "{events}");
    let said: Vec<&str> = events.stdout.lines().collect();
    assert_eq!(said.len(), 1, "one run past its limit: {events}");
    let fields: Vec<&str> = said[0].split(' ').collect();
    assert_eq!(fields.get(..2), Some(&["forked", "4"][..]), "{events}");
    let refused: u64 = fields.get(2).and_then(|n| n.parse().ok()).unwrap_or(0);
    assert!(refused > 0, "{events}");
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    assert!(
        log.contains("past its pids limit, a process or thread was refused: pids.max 4"),
        "{log}"
    );
    // A joiner's own limit, in its own cgroup.
    let provider = run_in(&home, &image, &["-d", "--name", "provider"], &["sleep"]);
    assert_eq!(provider.status, Some(0), "{provider}");
    let since = now().to_string();
    let joined = run_in(
        &home,
        &image,
        &[
            "--rm",
            "--name",
            "joined",
            "--network",
            "container:provider",
            "--pids-limit",
            "4",
        ],
        &["burn", "8", "100"],
    );
    assert!(joined.stdout.contains("of 8 threads"), "{joined}");
    let until = (now() + 1).to_string();
    let events = shards_in(
        &home,
        &[
            "events",
            "--since",
            &since,
            "--until",
            &until,
            "--filter",
            "event=pids-limit",
            "--format",
            r#"{{.Actor.Attributes.name}} {{index .Actor.Attributes "pids.max"}}"#,
        ],
    );
    assert_eq!(events.stdout, "joined 4\n", "{events}");
    let removed = shards_in(&home, &["rm", "-f", "provider"]);
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// `update -m` moves a running microVM's memory both ways (the user's plan, item 3): every
/// run plugs memory as it needs it up to its limit (D148), and an update tells its VM its
/// new most before its guest's cgroup, which its guest then reports it may plug (D149).
/// Past `-m 256m` a workload of 512 MiB is killed; raised to `1g`, it holds them, and its
/// microVM reports it may have a gigabyte; lowered again, it is killed again.
#[test]
fn update_moves_a_microvms_memory_both_ways() {
    let Some((home, image)) = home("containers-update-memory") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let mut held = start(
        &home,
        &image,
        &["--name", "grows", "-m", "256m", "--memory-swap", "256m"],
        &["sleep"],
    );
    let alloc = || shards(&["exec", "grows", "/bin/testguest", "alloc", "512"]);
    let killed = alloc();
    assert_eq!(killed.status, Some(137), "{killed}");
    let raised = shards(&["update", "-m", "1g", "--memory-swap", "1g", "grows"]);
    assert_eq!(raised.status, Some(0), "{raised}");
    let held_more = alloc();
    assert_eq!(
        (held_more.status, held_more.stdout.as_str()),
        (Some(0), "allocated 512\n"),
        "{held_more}{}",
        oom_said(&home)
    );
    // What it reports it has: a gigabyte, its kernel's own aside, where it booted with
    // what a run without a limit boots with.
    let meminfo = shards(&["exec", "grows", "/bin/testguest", "meminfo", "MemTotal"]);
    let total: u64 = meminfo
        .stdout
        .trim()
        .strip_prefix("MemTotal:")
        .and_then(|v| v.trim().strip_suffix("kB"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    assert!((1 << 20..2 << 20).contains(&total), "{meminfo}");
    let lowered = shards(&["update", "-m", "256m", "--memory-swap", "256m", "grows"]);
    assert_eq!(lowered.status, Some(0), "{lowered}");
    let killed = alloc();
    assert_eq!(killed.status, Some(137), "{killed}");
    assert_eq!(shards(&["stop", "-t", "1", "grows"]).status, Some(0));
    let _ = exit(&mut held);
}

/// `update`, as `docker update` takes it: a running microVM's limits written to its
/// workload's cgroup as it runs, and kept; dockerd's refusals; the restart policy.
#[test]
fn update_changes_limits_of_running_microvms() {
    let Some((home, image)) = home("containers-update") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let mut held = start(&home, &image, &["--name", "held"], &["sleep"]);
    let refused = shards(&["update", "-m", "64m", "held"]);
    assert_eq!(
        (refused.status, refused.stderr),
        (
            Some(1),
            "shards: cannot update held: --memory 67108864: past its --memory-swap 0, the memory and the swap together: give --memory-swap too\n".to_string()
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
        "/sys/fs/cgroup/cpuset.cpus.effective",
    ]);
    // Its CPUs as many as --cpus 1.5 needs, of its microVM's, which has all the host's (D191).
    let host_cpus = std::thread::available_parallelism().unwrap().get();
    let first = |n: usize| {
        if n == 1 {
            "0".to_string()
        } else {
            format!("0-{}", n - 1)
        }
    };
    let given = format!("{}\\n", first(host_cpus.min(2)));
    let values: Vec<&str> = read.stdout.lines().filter_map(|l| l.strip_prefix("= ")).collect();
    assert_eq!(
        values,
        [
            "67108864\\n",
            "67108864\\n",
            "150000 100000\\n",
            "50\\n",
            given.as_str()
        ],
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
    // And every one of them again, as its limit rises to the host's.
    let every = shards(&["update", "--cpus", &host_cpus.to_string(), "held"]);
    assert_eq!(every.status, Some(0), "{every}");
    let read = shards(&[
        "exec",
        "held",
        "/bin/testguest",
        "slurp",
        "/sys/fs/cgroup/cpuset.cpus.effective",
        "/sys/fs/cgroup/cpu.max",
    ]);
    assert_eq!(
        read.stdout,
        format!(
            "slurp /sys/fs/cgroup/cpuset.cpus.effective: {}\nslurp /sys/fs/cgroup/cpu.max: {} 100000\n",
            first(host_cpus),
            host_cpus * 100_000
        ),
        "{read}"
    );
    for (args, said) in [
        (
            &["--cpu-period", "50000", "held"][..],
            "shards: cannot update held: --cpu-period: its CPU time is set by --cpus already: update --cpus\n".to_string(),
        ),
        (
            &["-m", "1m", "held"][..],
            "shards: --memory 1048576: a run's memory is at least 6 MiB\n".to_string(),
        ),
        (
            &["--blkio-weight", "5", "held"][..],
            "shards: --blkio-weight 5: from 10 to 1000\n".to_string(),
        ),
        (
            &["--cpus", "1", "nope"][..],
            "shards: No such container: nope\n".to_string(),
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
#[test]
fn microvm_sizes_are_listed() {
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
#[test]
fn import_makes_images_of_tarballs() {
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
    let mut zipping = flate2::write::GzEncoder::new(
        std::fs::File::create(&gz).unwrap(),
        flate2::Compression::default(),
    );
    std::io::copy(&mut std::fs::File::open(&tar).unwrap(), &mut zipping).unwrap();
    zipping.finish().unwrap();
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
    // Short on Unix, where a socket's path fits sun_path: the home's is long on macOS.
    let socket = Socket(if cfg!(windows) {
        home.join("engine.sock")
    } else {
        format!("/tmp/shards-engine-{}.sock", std::process::id()).into()
    });
    let _ = std::fs::remove_file(&socket.0);
    let engine = shards_vmm::platform::UnixListener::bind(&socket.0).unwrap();
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
        .unwrap_or_else(|e| {
            // Its publisher's errors go to the daemon's log alone (local_store.rs).
            let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(60).collect();
            panic!(
                "the import published nothing: {e:?}; the daemon's log's last lines, last first: {tail:#?}"
            )
        });
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
            // Go's words for the error each OS gives (syscall's Errno), as docker/cli's are.
            if cfg!(windows) {
                "open nope.tar: The system cannot find the file specified.\n"
            } else {
                "open nope.tar: no such file or directory\n"
            },
        ),
        (
            &["import", "-c", "RUN x", tar.to_str().unwrap()][..],
            "shards: RUN is not a valid change command\n",
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
fn security_opt_confines_the_workload() {
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
            "shards: invalid --security-opt 2: \"no-new-privileges=yes\"",
        ),
        ("bogus=1", "shards: invalid --security-opt 2: \"bogus=1\""),
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
fn workloads_start_with_high_file_and_process_limits() {
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
fn stats_read_each_microvms_guest() {
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
fn devices_are_given_and_confined() {
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
        format!("shards: {missing}\nfailed to start containers: nodev\n")
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
fn block_io_limits_hold() {
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
    // unlimited it takes milliseconds, as the guest times its read: a run's start and end on
    // a busy runner took longer than the held read whole (3.75 s against 2.97 s, x86_64 musl
    // CI 40197051482, PM M336).
    let read = |extra: &[&str]| {
        let mut o = vec!["--rm", "-u", "0", "--device", "/dev/pmem0"];
        o.extend_from_slice(extra);
        let r = run_in(&home, &image, &o, &["fs", "readn:1048576:/dev/pmem0"]);
        assert_eq!(r.status, Some(0), "{r}");
        let us = r
            .stdout
            .lines()
            .find_map(|l| {
                l.strip_prefix("readn 1048576 in ")?
                    .strip_suffix(" us")?
                    .parse()
                    .ok()
            })
            .unwrap_or_else(|| panic!("no read's time: {r}"));
        std::time::Duration::from_micros(us)
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
fn attach_joins_a_running_microvm() {
    use std::io::Write as _;
    let Some((home, image)) = home("containers-attach") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    // Under --init, so that it ends of the SIGTERM it does not handle, which as PID 1 it
    // would not hear.
    let made = run_in(&home, &image, &["-d", "-i", "--name", "echo", "--init"], &["cat"]);
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
        (Some(1), "shards: No such container: nope\n")
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
            "-it: stdin is not a terminal, so it cannot be the command's: leave out -t, or run it from a terminal\n"
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

/// A network's options are what its microVMs do: one made with `enable_icc` false keeps
/// its members from one another, their names still answered, as dockerd's bridge keeps
/// them; its ports published with no address are bound at its `host_binding_ipv4` alone,
/// and reached there; an MTU is kept as given, its members keeping their devices'; a
/// routed one publishes no port; and each option is read as dockerd reads it. The default
/// network says what it does.
#[test]
fn a_networks_options_are_what_its_microvms_do() {
    let Some((home, image)) = home("containers-netopts") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let options = shards(&["network", "inspect", "-f", "{{json .Options}}", "bridge"]);
    let options: serde_json::Value = serde_json::from_str(&options.stdout).unwrap();
    assert_eq!(
        options["com.docker.network.bridge.enable_icc"], "false",
        "{options}"
    );
    assert_eq!(options["com.docker.network.driver.mtu"], "65520", "{options}");
    assert_eq!(
        options["com.docker.network.bridge.host_binding_ipv4"], "0.0.0.0",
        "{options}"
    );
    assert!(
        options.get("com.docker.network.bridge.name").is_none(),
        "{options}"
    );
    // A home whose record says what an older shards said of it is told the truth too.
    for entry in std::fs::read_dir(home.join("networks")).unwrap() {
        let path = entry.unwrap().path();
        let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if record["name"] == "bridge" {
            record["options"]["com.docker.network.bridge.enable_icc"] = "true".into();
            record["options"]["com.docker.network.driver.mtu"] = "1500".into();
            record["options"]["com.docker.network.bridge.name"] = "docker0".into();
            std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        }
    }
    let again = shards(&["network", "inspect", "-f", "{{json .Options}}", "bridge"]);
    let again: serde_json::Value = serde_json::from_str(&again.stdout).unwrap();
    assert_eq!(again, options);
    for (option, said) in [
        (
            "com.docker.network.driver.mtu=abc",
            "failed to parse com.docker.network.driver.mtu value: abc (strconv.Atoi: parsing \"abc\": invalid syntax)",
        ),
        (
            "com.docker.network.driver.mtu=99999999999999999999",
            "(strconv.Atoi: parsing \"99999999999999999999\": value out of range)",
        ),
        ("com.docker.network.driver.mtu=-1", "invalid MTU number: -1"),
        (
            "com.docker.network.bridge.enable_ip_masquerade=yes",
            "failed to parse com.docker.network.bridge.enable_ip_masquerade value: yes (strconv.ParseBool: parsing \"yes\": invalid syntax)",
        ),
        (
            "com.docker.network.bridge.gateway_mode_ipv4=direct",
            "failed to parse com.docker.network.bridge.gateway_mode_ipv4 value: direct (unknown gateway mode direct)",
        ),
        (
            "com.docker.network.bridge.host_binding_ipv4=localhost",
            "failed to parse com.docker.network.bridge.host_binding_ipv4 value: localhost (nil ip)",
        ),
        (
            "com.docker.network.bridge.gateway_mode_ipv6=isolated",
            "gateway mode 'isolated' can only be used for an internal network",
        ),
    ] {
        let r = shards(&["network", "create", "-o", option, "refused"]);
        assert_eq!(r.status, Some(1), "{option}: {r}");
        assert!(r.stderr.contains(said), "{option}: {r}");
    }
    let made = shards(&[
        "network",
        "create",
        "--subnet",
        "10.78.0.0/24",
        "-o",
        "com.docker.network.bridge.enable_icc=false",
        "-o",
        "com.docker.network.bridge.host_binding_ipv4=127.0.0.1",
        "-o",
        "com.docker.network.driver.mtu=1400",
        "apart",
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let mut srv = start(
        &home,
        &image,
        &["--name", "lone", "--network", "apart", "-p", "7000"],
        &["serve", "7000", "1"],
    );
    // Its name answers, and nothing reaches it.
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "apart"],
        &["resolve", "lone"],
    );
    assert!(r.stdout.starts_with("resolve 10.78.0."), "{r}");
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "apart"],
        &["ask", "lone:7000"],
    );
    assert_ne!(r.status, Some(0), "{r}");
    // Its port at the host's loopback alone, and reached there.
    let ports = shards(&[
        "inspect",
        "-f",
        "{{range $p, $b := .NetworkSettings.Ports}}{{range $b}}{{.HostIp}} {{.HostPort}}\n{{end}}{{end}}",
        "lone",
    ]);
    let bound: Vec<&str> = ports.stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(bound.len(), 1, "{ports}");
    let port: u16 = bound[0].strip_prefix("127.0.0.1 ").unwrap().parse().unwrap();
    let mut reply = String::new();
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    conn.shutdown(std::net::Shutdown::Write).unwrap();
    std::io::Read::read_to_string(&mut conn, &mut reply).unwrap();
    assert!(reply.starts_with("from "), "{reply:?}");
    assert!(srv.wait().unwrap().success());
    // The MTU kept as given, its members their devices'.
    let mtu = shards(&[
        "network",
        "inspect",
        "-f",
        "{{index .Options \"com.docker.network.driver.mtu\"}}",
        "apart",
    ]);
    assert_eq!(mtu.stdout, "1400\n", "{mtu}");
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "apart"],
        &["fs", "print:/sys/class/net/eth0/mtu"],
    );
    assert_eq!(r.stdout, "65520\n", "{r}");
    // A routed network publishes nothing on the host.
    let made = shards(&[
        "network",
        "create",
        "--subnet",
        "10.79.0.0/24",
        "-o",
        "com.docker.network.bridge.gateway_mode_ipv4=routed",
        "direct",
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let r = run_in(
        &home,
        &image,
        &["--rm", "--network", "direct", "-p", "7000"],
        &["exit", "0"],
    );
    assert_ne!(r.status, Some(0), "{r}");
    assert!(r.stderr.contains("its gateway mode is routed"), "{r}");
}

/// D46: microVMs on a user network reach one another, by address and by each name the
/// network gives a member (its name, aliases, short ID and host name, any case, and each
/// as `name.network`), answered at 127.0.0.11 with a TTL of 0, a name past the network
/// that no grant reaches refused at once; the server sees each by its own address; a
/// microVM off the network reaches none of them; and `shards network` creates, lists,
/// inspects, refuses and removes networks in Docker's words (probed on Docker Engine
/// 29.3.1).
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
    // Kept by no cache; the name an address gives back resolving forward; and a name past
    // the network, which no grant reaches, refused at once.
    for (name, said) in [
        ("web", "dnsttl 0 10.77.0.50\n"),
        ("srv.lan", "dnsttl 0 10.77.0.50\n"),
        ("example.com", "dnsttl rcode 5\n"),
    ] {
        let r = run_in(&home, &image, &["--rm", "--network", "lan"], &["dnsttl", name]);
        assert_eq!(r.stdout, said, "{name}: {r}");
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
        r.stderr
            .contains("--network lan: 10.9.9.9: in none of the network's subnets"),
        "{r}"
    );
    let rm = shards(&["network", "rm", "lan"]);
    assert_eq!(rm.status, Some(1), "{rm}");
    assert!(
        rm.stderr
            .starts_with("shards: cannot remove network lan: runs are on it (name:\"srv\" id:\""),
        "{rm}"
    );
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
            "shards: a network is named lan already\n",
        ),
        (
            &["network", "create", "bridge"],
            "shards: bridge is one of shards' own networks, bridge, host and none: give yours another name\n",
        ),
        (
            &["network", "create", "--subnet", "10.77.0.0/24", "other"],
            "shards: invalid pool request: Pool overlaps with other one on this address space\n",
        ),
        (
            &["network", "create", "--gateway", "1.2.3.4", "other"],
            "--ip-range and --gateway each belong to a --subnet: give its subnet\n",
        ),
        (
            &["network", "rm", "bridge"],
            "shards: bridge is one of shards' own networks, and stays\nexit status 1\n",
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
fn image_configs_run_key_by_key() {
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
    let hc = "the image's config does not read: json: cannot unmarshal number into Go struct field DockerOCIImageConfig.config.DockerOCIImageConfigExt.Healthcheck of type v1.HealthcheckConfig";
    let ran = run_in(&home, "badhc:1", &["--rm"], &["report"]);
    assert_eq!(
        (ran.status, ran.stdout.as_str(), ran.stderr.as_str()),
        (
            Some(125),
            "",
            format!("shards: {hc}\n\nRun 'shards run --help' for more information\n").as_str()
        ),
        "{ran}"
    );
    for args in [&["create", "badhc:1"][..], &["tag", "badhc:1", "other:1"]] {
        let said = shards(args);
        assert_eq!(
            (said.status, said.stderr.as_str()),
            (Some(1), format!("shards: {hc}\n").as_str()),
            "{args:?}: {said}"
        );
    }
    for args in [&["image", "inspect", "badhc:1"][..], &["inspect", "badhc:1"]] {
        let said = shards(args);
        assert_eq!(
            (said.status, said.stdout.as_str(), said.stderr.as_str()),
            (Some(1), "[]\n", format!("shards: {hc}\n").as_str()),
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
            "shards: the image's config does not read: json: cannot unmarshal number into Go struct field .history of type []v1.History\n"
        ),
        "{history}"
    );
    let ran = run_in(&home, "badhist:1", &["--rm"], &["report"]);
    assert_eq!(
        (ran.status, ran.stderr.as_str()),
        (
            Some(125),
            "shards: the image's config does not read: json: cannot unmarshal number into Go struct field DockerOCIImage.Image.history of type []v1.History\n\nRun 'shards run --help' for more information\n"
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

/// A file's device and inode numbers, and a directory's, as `testguest fs ino:` printed
/// them: each path's `DEV:INO`, in order.
fn numbers(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let (path, n) = l.split_once(' ')?;
            Some((path.to_string(), n.to_string()))
        })
        .collect()
}

/// A run's directories keep their numbers for as long as they live, and its files and
/// directories share one device, as on Docker's one filesystem: its root's overlay maps
/// its layers' inode numbers into one space (init's run::XINO). Without it, the kernel
/// numbered a directory anew each time it dropped it from its caches, as it does here, and
/// GNU tar failed extracting a tree large enough for that to happen as it extracted it.
#[test]
fn a_runs_directories_keep_their_numbers() {
    let Some((home, image)) = home("containers-numbers") else {
        return;
    };
    let ops = [
        "fs",
        "mkdir:/d",
        "mkdir:/d/e",
        "write:/d/f=x",
        "ino:/d",
        "ino:/d/e",
        "ino:/d/f",
        "write:/proc/sys/vm/drop_caches=2",
        "ino:/d",
        "ino:/d/e",
        "ino:/d/f",
    ];
    let r = run_in(&home, &image, &["--rm", "--privileged", "-u", "0"], &ops);
    assert_eq!(r.status, Some(0), "{r}");
    let n = numbers(&r.stdout);
    assert_eq!(n.len(), 6, "{r}");
    assert_eq!(
        n[..3],
        n[3..],
        "numbers changed as the kernel dropped its caches: {r}"
    );
    let device = |(_, n): &(String, String)| n.split(':').next().map(str::to_string);
    assert!(
        n.iter().all(|e| device(e) == device(&n[0])),
        "more than one device: {r}"
    );
}

/// `--device` by CDI name (D146), resolved by the daemon as dockerd's CDI driver resolves
/// it as a container starts, from the specs of its CDI directories: a device's variables,
/// spec-wide and its own, over the workload's; its groups but root's; its node, made from
/// the guest's own device at its host path, owned by the process's user, its access the
/// spec's; its mount, a host directory bound read-only, nosuid and nodev. A name no spec
/// has fails the start, not the making; a hook, which runs on a container's host, is
/// refused.
#[test]
fn cdi_devices_are_given_to_microvms() {
    let Some((home, image)) = home("containers-cdi") else {
        return;
    };
    // The daemon reads CDI's directories as it starts: started again, with ours.
    assert_eq!(shards_in(&home, &["stop", "daemon"]).status, Some(0));
    let specs = TempDir::new("cdi-specs");
    let shared = TempDir::new("cdi-shared");
    std::fs::write(shared.join("hello"), "from the host\n").unwrap();
    let spec = serde_json::json!({
        // additionalGids are v0.7.0's.
        "cdiVersion": "0.7.0",
        "kind": "test.example/dev",
        "containerEdits": {"env": ["SPEC_WIDE=1"]},
        "devices": [
            {"name": "fuse", "containerEdits": {
                "env": ["CDI_DEV=fuse"],
                "deviceNodes": [{"path": "/dev/cdi-fuse", "hostPath": "/dev/fuse", "permissions": "rw"}],
                "additionalGids": [4242, 0]
            }},
            {"name": "shared", "containerEdits": {
                "mounts": [{"hostPath": shared.to_str().unwrap(), "containerPath": "/cdi",
                            "options": ["ro", "nosuid", "nodev", "bind"]}]
            }},
            {"name": "hook", "containerEdits": {
                "hooks": [{"hookName": "createRuntime", "path": "/bin/true"}]
            }}
        ]
    });
    std::fs::write(specs.join("test.json"), spec.to_string()).unwrap();
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_CDI_SPEC_DIRS", specs.as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let run = |options: &[&str], command: &[&str]| {
        let args = run_args(&image, options, command);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        shards(&args)
    };
    let given = ["--rm", "-u", "1000", "--device", "test.example/dev=fuse"];
    let r = run(&given, &["report"]);
    assert_eq!(r.status, Some(0), "{r}");
    for line in ["uid 1000\n", "env CDI_DEV=fuse\n", "env SPEC_WIDE=1\n"] {
        assert!(r.stdout.contains(line), "{line}: {r}");
    }
    let groups = r
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("groups "))
        .unwrap_or_default();
    assert!(groups.split(',').any(|g| g == "4242"), "{r}");
    // The node's owner, where its spec names none, is the process's.
    let gid = r
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("gid "))
        .unwrap_or_default()
        .to_string();
    let r = run(&given, &["stat", "/dev/cdi-fuse"]);
    assert_eq!(r.stdout, format!("/dev/cdi-fuse other 666 1000:{gid} 0\n"), "{r}");
    // Its host directory, a device of its own, as a mount's.
    {
        let mut both = given.to_vec();
        both.extend(["--device", "test.example/dev=shared"]);
        let r = run(
            &both,
            &[
                "fs",
                "open:r:/dev/cdi-fuse",
                "print:/cdi/hello",
                "print:/proc/self/mounts",
            ],
        );
        assert_eq!(r.status, Some(0), "{r}");
        assert!(r.stdout.starts_with("from the host\n"), "{r}");
        let cdi = r
            .stdout
            .lines()
            .find(|l| l.split(' ').nth(1) == Some("/cdi"))
            .unwrap_or_default();
        let options: Vec<&str> = cdi.split(' ').nth(3).unwrap_or_default().split(',').collect();
        for o in ["ro", "nosuid", "nodev"] {
            assert!(options.contains(&o), "{o}: {cdi}\n{r}");
        }
        let r = run(&both, &["fs", "write:/cdi/x=1"]);
        assert!(r.stderr.contains("Read-only file system"), "{r}");
    }
    // A name no spec has: made, then refused as it starts.
    let made = shards(&[
        "create",
        "--pull",
        "never",
        "--name",
        "nocdi",
        "--device",
        "test.example/dev=nope",
        &image,
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let started = shards(&["start", "nocdi"]);
    assert_eq!(started.status, Some(1), "{started}");
    assert!(
        started
            .stderr
            .contains("CDI device injection failed: unresolvable CDI devices test.example/dev=nope"),
        "{started}"
    );
    let r = run(&["--rm", "--device", "test.example/dev=hook"], &["exit", "0"]);
    assert!(
        r.stderr
            .contains("a createRuntime hook (/bin/true), which runs on a container's host"),
        "{r}"
    );
    // A joiner's node, owned by its user by name, which its own image's users say.
    let mut provider = start(&home, &image, &["--name", "cdiprov"], &["serve", "7000", "1"]);
    let r = run(
        &[
            "--rm",
            "--network",
            "container:cdiprov",
            "-u",
            "app",
            "--device",
            "test.example/dev=fuse",
        ],
        &["stat", "/dev/cdi-fuse"],
    );
    assert_eq!(r.stdout, "/dev/cdi-fuse other 666 1000:1000 0\n", "{r}");
    assert_eq!(shards(&["rm", "-f", "cdiprov"]).status, Some(0));
    let _ = provider.wait();
}

/// `mib` MiB of xorshift64's bytes at `path`, which no file system keeps as holes or shares.
fn noise(path: &Path, mib: usize) {
    use std::io::Write as _;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut block = vec![0u8; 1 << 20];
    for _ in 0..mib {
        for word in block.as_chunks_mut::<8>().0 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *word = x.to_le_bytes();
        }
        file.write_all(&block).unwrap();
    }
    file.flush().unwrap();
}

/// An image of gigabytes boots as a small one does. Its root is a pmem region, which
/// libnvdimm probes asynchronously, longest for the largest; init waits for its node
/// (init's pmem.rs), where a 2.1 GB image's runs found none and failed every time.
#[test]
fn an_image_of_gigabytes_boots() {
    let Some((home, image)) = home("containers-big") else {
        return;
    };
    let ctx = home.join("big");
    std::fs::create_dir(&ctx).unwrap();
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {image}\nCOPY big /big\n")).unwrap();
    noise(&ctx.join("big"), 2048);
    // Its build copies 2 GiB through the builder: past TIMEOUT on CI's x86_64 runners,
    // where it stays a guard against a hang.
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let built = run_shards_env(
        &[],
        &["build", "-q", "-t", "big:1", ctx.to_str().unwrap()],
        &env,
        Duration::from_secs(600),
    );
    assert_eq!(built.status, Some(0), "{built}");
    std::fs::remove_file(ctx.join("big")).unwrap();
    let ran = run_in(&home, "big:1", &["--rm"], &["fs", "open:r:/big"]);
    assert_eq!(ran.status, Some(0), "{ran}");
}

/// What a run's workload frees goes back to the host (D151): half a gigabyte touched, then
/// freed, and once the guest has reported it free, its microVM holds what an idle one
/// does, not the half gigabyte, as a container's memory freed is its host's at once.
#[test]
fn a_runs_memory_freed_goes_back_to_the_host() {
    // Its runs restore templates, which a host that restores no snapshot has none of.
    if common::cannot_pool() {
        return;
    }
    let Some((home, image)) = home("containers-giveback") else {
        return;
    };
    // Its runs restore templates, as a user's do: a daemon started with SHARDS_KERNEL
    // boots every run.
    assert_eq!(shards_in(&home, &["stop", "daemon"]).status, Some(0));
    let env: [(&str, &OsStr); 2] = [("SHARDS_HOME", home.as_os_str()), ("SHARDS_TIMING", "1".as_ref())];
    let (kernel, init) = (kernel(), guest_init());
    let recorded = run_shards_env(
        &["guest"],
        &[
            "use",
            "--kernel",
            kernel.to_str().unwrap(),
            "--init",
            init.to_str().unwrap(),
        ],
        &env,
        TIMEOUT,
    );
    assert_eq!(recorded.status, Some(0), "{recorded}");
    let run = |command: &[&str]| {
        let args = run_args(&image, &["--rm"], command);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run_shards_env(&[], &args, &env, TIMEOUT)
    };
    // The microVM that saves the template, then one restored from it, each idle as long
    // as the run that frees memory.
    let first = run(&["alloc-free", "0", "10"]);
    assert_eq!(first.status, Some(0), "{first}");
    let idle = run(&["alloc-free", "0", "10"]);
    assert_eq!(idle.status, Some(0), "{idle}");
    // The guest reports memory freed two seconds on, and of each free list about 64
    // blocks a pass, a pass each two seconds (mm/page_reporting.c): 512 MiB in two.
    let r = run(&["alloc-free", "512", "10"]);
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("freed 512"), "{r}");
    // Only where its kernel counts what a pass holds as free (`page_reporting.held`,
    // resources/kernel/patches/0006): elsewhere its init lets the device go as it boots,
    // and it reports nothing (PM M224).
    let counts = run(&["stat", "/sys/module/page_reporting/parameters/held"]);
    if counts.status != Some(0) {
        assert_eq!(r.reported_kib(), Some(0), "{r}");
        println!("SKIP: this guest kernel does not count what a pass holds as free (D151)");
        return;
    }
    // A template is saved with what its boot freed reported, where its kernel can be told
    // to report at once (`page_reporting.flush`, resources/kernel/patches/0005): a restore
    // reports none of it, two seconds into its life, which its runs paid for. Measured:
    // 0 KiB, or 4,096 where a block was freed in its 20 s, the pool's and its run's; with
    // the pass left to run, 212,992. Less than a pass's one batch of the smallest blocks
    // (32 of 2 MiB) is none of it.
    let flushes = run(&["stat", "/sys/module/page_reporting/parameters/flush"]);
    if flushes.status == Some(0) {
        assert!(
            idle.reported_kib().is_some_and(|kib| kib < 32 * (2 << 10)),
            "{idle}"
        );
    } else {
        println!("SKIP: this guest kernel cannot be told to report at once (D151)");
    }
    // Against a restore as long that frees nothing: the guest's own use, and what of it
    // it reports free in turn. Measured 94 MiB idle, and 57 and 132 MiB after 512 MiB
    // freed, a peak of 578 and 645 MiB; with none given back, the peak.
    let (idle, held) = (idle.footprint_kib().unwrap(), r.footprint_kib().unwrap());
    // A restore's RAM on Windows is its template's runs of data, a copy-on-write view each,
    // and memory of its own between them (D151, D196), where what its workload frees came
    // from and goes back to the host at its pages' grain (PM M378).
    assert!(
        held < idle + (512 << 10) / 2,
        "{held} KiB held of 512 MiB freed, {idle} KiB idle: {r}"
    );
}

/// A run's memory, plugged as its workload needs it (D148): without `-m` it holds more
/// than its microVM boots with, as a container may hold its host's, in the microVM that
/// saved its template and in a restore of it; with `-m` it is ended past its limit, as
/// Docker's is, and runs within it, its microVM booted with what its limit needs.
#[test]
fn a_runs_memory_grows_as_its_workload_needs_it() {
    let Some((home, image)) = home("containers-plug") else {
        return;
    };
    // Its runs boot the guest recorded, as a user's do, and so restore templates: a
    // daemon started with SHARDS_KERNEL boots every run.
    assert_eq!(shards_in(&home, &["stop", "daemon"]).status, Some(0));
    let env: [(&str, &OsStr); 1] = [("SHARDS_HOME", home.as_os_str())];
    let (kernel, init) = (kernel(), guest_init());
    let recorded = run_shards_env(
        &["guest"],
        &[
            "use",
            "--kernel",
            kernel.to_str().unwrap(),
            "--init",
            init.to_str().unwrap(),
        ],
        &env,
        TIMEOUT,
    );
    assert_eq!(recorded.status, Some(0), "{recorded}");
    let run = |options: &[&str], command: &[&str]| {
        let args = run_args(&image, options, command);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run_shards_env(&[], &args, &env, TIMEOUT)
    };
    let r = run(&["--rm"], &["alloc", "1024"]);
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("allocated 1024"), "{r}");
    let r = run(&["--rm", "-m", "256m"], &["alloc", "1024"]);
    assert_eq!(r.status, Some(137), "{r}");
    let r = run(&["--rm", "-m", "1g"], &["alloc", "512"]);
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("allocated 512"), "{r}");
    // A restored microVM grows as the one that saved its template did.
    let r = run(&["--rm"], &["alloc", "1024"]);
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("allocated 1024"), "{r}");
    // Its memory as it reports it is what it may have, where its kernel reports what it
    // may plug (D149), as a container's is its host's: not what it boots with, 243 MiB.
    // Each file's text: `PATH file MODE UID:GID SIZE`, then `= TEXT`, its newlines `\n`.
    let reported = |options: &[&str]| -> Option<(u64, String)> {
        let r = run(
            options,
            &["stat", "/proc/sys/vm/pluggable_kbytes", "/proc/meminfo"],
        );
        if r.stdout.contains("/proc/sys/vm/pluggable_kbytes missing") {
            return None;
        }
        assert_eq!(r.status, Some(0), "{r}");
        let text = |path: &str| -> String {
            let mut lines = r.stdout.lines();
            lines
                .by_ref()
                .find(|l| l.starts_with(&format!("{path} ")))
                .and(lines.next())
                .and_then(|l| l.strip_prefix("= "))
                .unwrap()
                .replace("\\n", "\n")
        };
        Some((
            text("/proc/sys/vm/pluggable_kbytes").trim().parse().unwrap(),
            text("/proc/meminfo"),
        ))
    };
    let kib = |meminfo: &str, field: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap()
    };
    match reported(&["--rm"]) {
        None => println!("SKIP: this guest kernel reports none of the memory it may plug (D149)"),
        Some((pluggable, meminfo)) => {
            let host = common::host_memory() / 1024;
            // All of it, never more than its host has; what may still be plugged is free
            // and available as well, as plugged it would be.
            let total = kib(&meminfo, "MemTotal:");
            assert!(
                total > host / 2 && total <= host,
                "MemTotal {total} kB, the host's {host} kB: {meminfo}"
            );
            assert!(
                pluggable > 0
                    && kib(&meminfo, "MemFree:") > pluggable
                    && kib(&meminfo, "MemAvailable:") > pluggable,
                "{pluggable} kB pluggable: {meminfo}"
            );
            // One allocation of four times what it boots with, which the kernel's
            // overcommit heuristic refused while it counted only memory plugged.
            let r = run(&["--rm"], &["alloc-once", "1024"]);
            assert_eq!(r.status, Some(0), "{r}");
            assert!(r.stdout.contains("allocated 1024 at once"), "{r}");
            // A run with a limit boots as one without, and reports its own memory, what it
            // may plug its limit's, its template's most lowered to its own as it begins
            // (item 3: every limit's runs restore one template).
            let (pluggable, meminfo) = reported(&["--rm", "-m", "1g"]).unwrap();
            let total = kib(&meminfo, "MemTotal:");
            assert!(
                pluggable > 0 && (1 << 20..2 << 20).contains(&total),
                "{pluggable} kB pluggable: {meminfo}"
            );
        }
    }
    // The templates, each a directory: the daemon's record of each is a file beside it.
    // The runs with a limit and without share one, each told its own most to plug.
    if common::cannot_pool() {
        return;
    }
    let templates: Vec<_> = std::fs::read_dir(home.join("templates"))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name())
        .collect();
    assert_eq!(templates.len(), 1, "{templates:?}");
}

/// A run's CPUs, real-time time, isolation and runtime as its microVM has them (D155):
/// real-time scheduling for a workload let have it (`--cap-add sys_nice`), within the
/// guest's real-time budget, which `--cpu-rt-period` and `--cpu-rt-runtime` set, the run's
/// alone in its microVM; `--cpu-count` its CPUs, of its microVM's, which has all the
/// host's (D191); `--cpu-percent` the share of the host's CPU time its workload may use, on
/// CPUs enough for it; isolation by its microVM,
/// `default` or `hyperv`, never a kernel shared; its runtime the microVM, by runc's names
/// or Kata's. Its annotations kept, shown and matched; each refusal in shards' words.
#[test]
fn a_microvm_takes_its_cpus_real_time_isolation_and_annotations() {
    let Some((home, image)) = home("containers-microvm-cpus") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let host = std::thread::available_parallelism().unwrap().get();
    // Real-time scheduling: root's with CAP_SYS_NICE, and the image's user's within its
    // RLIMIT_RTPRIO; root's without the capability, and the user's without the limit,
    // refused (EPERM), as the kernel decides it (kernel/sched/syscalls.c).
    for (options, said) in [
        (&["--user", "root", "--cap-add", "sys_nice"][..], "fifo: ok\n"),
        (&["--ulimit", "rtprio=1"], "fifo: ok\n"),
        (&["--user", "root"], "fifo: errno 1\n"),
        (&[], "fifo: errno 1\n"),
    ] {
        let mut flags = vec!["--rm"];
        flags.extend(options);
        let fifo = run_in(&home, &image, &flags, &["fifo"]);
        assert_eq!(
            (fifo.status, fifo.stdout.as_str()),
            (Some(0), said),
            "{options:?}: {fifo}"
        );
    }
    let budget = run_in(
        &home,
        &image,
        &["--rm", "--cpu-rt-period", "100000", "--cpu-rt-runtime", "50000"],
        &[
            "slurp",
            "/proc/sys/kernel/sched_rt_period_us",
            "/proc/sys/kernel/sched_rt_runtime_us",
        ],
    );
    assert_eq!(
        budget.stdout,
        "slurp /proc/sys/kernel/sched_rt_period_us: 100000\nslurp /proc/sys/kernel/sched_rt_runtime_us: 50000\n",
        "{budget}"
    );
    // A runtime alone, past the guest kernel's own period: refused as the run starts.
    let past = run_in(
        &home,
        &image,
        &["--rm", "--cpu-rt-runtime", "2000000"],
        &["exit", "0"],
    );
    assert_ne!(past.status, Some(0), "{past}");
    assert!(
        past.stderr
            .contains("--cpu-rt-runtime 2000000 is more than this guest's real-time period, 1000000 microseconds: give --cpu-rt-period too"),
        "{past}"
    );
    if host >= 2 {
        // The first two of its microVM's CPUs, which are all the host's (D191).
        let two = run_in(
            &home,
            &image,
            &["--rm", "--cpu-count", "2"],
            &[
                "slurp",
                "/sys/fs/cgroup/cpuset.cpus.effective",
                "/sys/devices/system/cpu/online",
            ],
        );
        assert_eq!(
            two.stdout,
            format!(
                "slurp /sys/fs/cgroup/cpuset.cpus.effective: 0-1\nslurp /sys/devices/system/cpu/online: 0-{}\n",
                host - 1
            ),
            "{two}"
        );
    }
    let half = run_in(
        &home,
        &image,
        &["--rm", "--cpu-percent", "50"],
        &[
            "slurp",
            "/sys/fs/cgroup/cpu.max",
            "/sys/fs/cgroup/cpuset.cpus.effective",
        ],
    );
    // On as many of its microVM's CPUs as its share needs, of all the host's (D191).
    let cpus = (host * 50).div_ceil(100);
    let given = if cpus == 1 {
        "0".to_string()
    } else {
        format!("0-{}", cpus - 1)
    };
    assert_eq!(
        half.stdout,
        format!(
            "slurp /sys/fs/cgroup/cpu.max: {} 100000\nslurp /sys/fs/cgroup/cpuset.cpus.effective: {given}\n",
            50 * host * 1000
        ),
        "{half}"
    );
    for (flag, value) in [
        ("--isolation", "hyperv"),
        ("--isolation", "DEFAULT"),
        ("--runtime", "runc"),
        ("--runtime", "io.containerd.runc.v2"),
        ("--runtime", "kata"),
        ("--runtime", "io.containerd.kata.v2"),
    ] {
        let ran = run_in(&home, &image, &["--rm", flag, value], &["exit", "0"]);
        assert_eq!(ran.status, Some(0), "{flag} {value}: {ran}");
    }
    let made = shards(&[
        "create",
        "--pull",
        "never",
        "--name",
        "annotated",
        "--annotation",
        "a=1",
        "--annotation",
        "b",
        "--annotation",
        "a=2",
        "--isolation",
        "hyperv",
        "--cpu-count",
        "1",
        "--cpu-rt-period",
        "100000",
        "--cpu-rt-runtime",
        "50000",
        &image,
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let shown = shards(&[
        "inspect",
        "-f",
        "{{json .HostConfig.Annotations}} {{.HostConfig.Isolation}} {{.HostConfig.Runtime}} {{.HostConfig.CpuCount}} {{.HostConfig.CpuRealtimePeriod}} {{.HostConfig.CpuRealtimeRuntime}}",
        "annotated",
    ]);
    assert_eq!(
        shown.stdout, "{\"a\":\"2\",\"b\":\"\"} hyperv shards 1 100000 50000\n",
        "{shown}"
    );
    for (filter, listed) in [
        ("annotation=a=2", "annotated\n"),
        ("annotation=b", "annotated\n"),
        ("annotation=a=1", ""),
        ("annotation=c", ""),
    ] {
        let ps = shards(&["ps", "-a", "--filter", filter, "--format", "{{.Names}}"]);
        assert_eq!(
            (ps.status, ps.stdout.as_str()),
            (Some(0), listed),
            "{filter}: {ps}"
        );
    }
    let past_host = (host + 1).to_string();
    let refusals: Vec<(Vec<&str>, String)> = vec![
        (
            vec!["--isolation", "process"],
            "--isolation process: every run is isolated in a microVM of its own, which shares no kernel with its host or another run: its isolation is default, or hyperv, a VM's".into(),
        ),
        (
            vec!["--runtime", "nope"],
            "--runtime nope: every run is a microVM of its own, whose runtime is shards (asked for by runc's or Kata's names too)".into(),
        ),
        (
            vec!["--cpu-count", &past_host],
            format!("--cpu-count {past_host}: a microVM has from 1 vCPU to as many as its host has CPUs, {host}"),
        ),
        (
            vec!["--cpu-percent", "101"],
            "--cpu-percent 101: a share of the host's CPU time is from 1 to 100 percent".into(),
        ),
        (
            vec!["--cpu-percent", "50", "--cpus", "1"],
            "--cpu-percent and --cpus each cap the microVM's CPU time: give one".into(),
        ),
        (
            vec!["--cpu-rt-period", "100000", "--cpu-rt-runtime", "150000"],
            "--cpu-rt-runtime 150000 is more than its period, --cpu-rt-period 100000".into(),
        ),
        (
            vec!["--annotation", "=x"],
            "--annotation =x: an annotation needs a key".into(),
        ),
    ];
    for (flags, said) in refusals {
        let mut args = vec!["create", "--pull", "never"];
        args.extend(&flags);
        args.push(&image);
        let refused = shards(&args);
        let want = format!("shards: {said}\n");
        assert_eq!(
            (refused.status, refused.stderr.as_str()),
            (Some(1), want.as_str()),
            "{flags:?}: {refused}"
        );
    }
    // Nothing is left of the refused.
    let listed = shards(&["ps", "-a", "--format", "{{.Names}}"]);
    assert_eq!(listed.stdout, "annotated\n", "{listed}");
}

/// A run's CPUs are held from its host too (D191): a workload root of its own guest
/// (`--privileged --cgroupns host`) that writes itself into its microVM's cgroup root,
/// out of the run's cpuset, where its guest gives it every CPU of the host's, uses no more
/// of the host's CPU time than its limit's CPUs, its VM process held there; and a run
/// without a limit is not held.
#[test]
fn a_workload_past_its_cgroup_is_held_to_its_cpus_by_its_host() {
    let Some((home, image)) = home("containers-cpu-hold") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let host = std::thread::available_parallelism().unwrap().get();
    if host < 4 {
        eprintln!("SKIP: {host} CPUs, which hold four burning threads already");
        return;
    }
    // Four threads for three seconds, past the run's cgroup: the VM process's CPU time
    // over the burn, and its wall time.
    let burn = |name: &str| {
        let pid: u32 = shards(&["inspect", "-f", "{{.State.Pid}}", name])
            .stdout
            .trim()
            .parse()
            .unwrap();
        let before = shards_vmm::platform::process_usage(pid).unwrap().cpu_ns;
        let t0 = Instant::now();
        // As the guest's root, which a privileged run's workload may become.
        let burned = shards(&[
            "exec",
            "-u",
            "root",
            name,
            "/bin/testguest",
            "burn",
            "4",
            "3000",
            "escape",
        ]);
        let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
        let used = shards_vmm::platform::process_usage(pid).unwrap().cpu_ns - before;
        assert_eq!(burned.stdout, "cgroup 0::/\nburned\n", "{burned}");
        (used, wall)
    };
    let options = |name: &'static str, limit: &'static [&'static str]| {
        let mut all = vec!["--name", name, "--privileged", "--cgroupns", "host"];
        all.extend(limit);
        all
    };
    let mut limited = start(&home, &image, &options("held", &["--cpus", "1"]), &["sleep"]);
    let (used, wall) = burn("held");
    // One CPU's time over the burn; what the burn may run of its first period, its
    // microVM quiet till then; and the VM process's own threads, its devices'.
    assert!(
        used <= wall + 1_000_000_000,
        "{used} ns of CPU in {wall} ns, held to one CPU"
    );
    let mut free = start(&home, &image, &options("free", &[]), &["sleep"]);
    // What the host gives four busy threads of a program's now: a busy guest's vCPUs share
    // its CPUs as its programs do (D4, PM M317), so four without a limit show more than a
    // CPU's time only on a host that gives a program's four that much.
    let given: f64 = std::thread::scope(|s| {
        let spun: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    let clock = shards_vmm::platform::ThreadClock::current().unwrap();
                    let until = Instant::now() + Duration::from_secs(1);
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    clock.read().unwrap()
                })
            })
            .collect();
        spun.into_iter().map(|t| t.join().unwrap() as f64 / 1e9).sum()
    });
    let (used, wall) = burn("free");
    // Four threads' time, on a host that gives it: more than a CPU's.
    if given >= 2.0 {
        assert!(
            used > wall + 1_000_000_000,
            "{used} ns of CPU in {wall} ns, without a limit, where the host gave a program's four threads {given:.2} CPUs"
        );
    } else {
        eprintln!(
            "SKIP: without a limit, {used} ns of CPU in {wall} ns: the host gave a program's four threads {given:.2} CPUs, too few to show a hold of one"
        );
    }
    assert_eq!(shards(&["rm", "-f", "held", "free"]).status, Some(0));
    let _ = exit(&mut limited);
    let _ = exit(&mut free);
}

/// A run's busy vCPUs are never depressed by XNU's real-time fail-safe (D4, PM M317):
/// with two threads of its guest busy for five seconds, no vCPU thread of its VM process
/// is at priority 0 at any look, and the busy ones go to timeshare while they run. Without
/// the governor's watch, a vCPU busy for a second is depressed and stays so.
#[cfg(target_os = "macos")]
#[test]
fn a_microvms_busy_vcpus_are_never_depressed_by_the_real_time_fail_safe() {
    let Some((home, image)) = home("containers-fail-safe") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let mut busy = start(&home, &image, &["--name", "busy"], &["sleep"]);
    let pid: i32 = shards(&["inspect", "-f", "{{.State.Pid}}", "busy"])
        .stdout
        .trim()
        .parse()
        .unwrap();
    // Every 50 ms, each vCPU thread of the VM process: its policy (1 timeshare) and base
    // priority, as proc_pidinfo says them (PROC_PIDLISTTHREADS, PROC_PIDTHREADINFO).
    let looking = std::thread::spawn(move || {
        const PROC_PIDLISTTHREADS: i32 = 6;
        let size = |n: usize| i32::try_from(n).unwrap();
        let mut seen = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(5500) {
            let mut ids = [0u64; 256];
            // SAFETY: a buffer of `ids`' size for the process's thread handles.
            let n = unsafe {
                libc::proc_pidinfo(
                    pid,
                    PROC_PIDLISTTHREADS,
                    0,
                    ids.as_mut_ptr().cast(),
                    size(size_of_val(&ids)),
                )
            };
            for &id in ids.iter().take(usize::try_from(n).unwrap_or(0) / 8) {
                // SAFETY: zeroed plain data, which proc_pidinfo fills.
                let mut t: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
                // SAFETY: a buffer of `t`'s size for the thread's information.
                let got = unsafe {
                    libc::proc_pidinfo(
                        pid,
                        libc::PROC_PIDTHREADINFO,
                        id,
                        (&raw mut t).cast(),
                        size(size_of_val(&t)),
                    )
                };
                // SAFETY: a name proc_pidinfo wrote, NUL-terminated within its array.
                let name = unsafe { std::ffi::CStr::from_ptr(t.pth_name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                if got == size(size_of_val(&t)) && name.starts_with("vcpu") {
                    seen.push((start.elapsed().as_millis(), name, t.pth_policy, t.pth_priority));
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        seen
    });
    let burned = shards(&["exec", "busy", "/bin/testguest", "burn", "2", "5000"]);
    let seen = looking.join().unwrap();
    assert!(burned.stdout.ends_with("burned\n"), "{burned}");
    assert!(!seen.is_empty(), "no vCPU thread of VM process {pid} seen");
    let depressed: Vec<_> = seen.iter().filter(|s| s.3 == 0).collect();
    assert!(
        depressed.is_empty(),
        "vCPU threads depressed to priority 0: {depressed:?}"
    );
    assert!(
        seen.iter().any(|s| s.2 == 1),
        "no busy vCPU went to timeshare: {:?}",
        seen.iter().filter(|s| s.0 > 2000).take(40).collect::<Vec<_>>()
    );
    assert_eq!(shards(&["rm", "-f", "busy"]).status, Some(0));
    let _ = exit(&mut busy);
}

/// A run's swap and memory as its microVM has them (D159): swap as its limits allow,
/// on a sparse file of its own past its memory (as much again by default), which its
/// workload uses past its memory where no swap would have it killed; its swappiness, the
/// microVM's own; and `--oom-kill-disable` a throttle at its memory, never the OOM
/// killer. Shown by `inspect` as given.
#[test]
fn a_microvm_swaps_and_throttles_as_its_limits_say() {
    let Some((home, image)) = home("containers-microvm-swap") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let run = |flags: &[&str], cmd: &[&str]| {
        let mut all = vec!["--rm"];
        all.extend(flags);
        run_in(&home, &image, &all, cmd)
    };
    // Swap as much again as its memory, by default, held to it by its cgroup: the
    // workload goes past its memory.
    let allowed = |flags: &[&str]| {
        let r = run(flags, &["lines", "/sys/fs/cgroup/memory.swap.max", ""]);
        let total = run(flags, &["meminfo", "SwapTotal"]);
        (r.stdout, total.stdout != "SwapTotal: 0 kB\n")
    };
    assert_eq!(allowed(&["-m", "64m"]), ("67108864\n".to_string(), true));
    // Each time: its swap's writes are done as the kernel makes them, so reclaim under its
    // limit never waits on its host, where a block device's left one run in four killed
    // within its swap (PM M230).
    for _ in 0..5 {
        let past = run(&["-m", "64m"], &["alloc", "96"]);
        assert_eq!(
            (past.status, past.stdout.as_str()),
            (Some(0), "allocated 96\n"),
            "{past}"
        );
    }
    // None without a memory limit: its microVM grows to the host's memory instead (D148).
    let none = run(&[], &["meminfo", "SwapTotal"]);
    assert_eq!(none.stdout, "SwapTotal: 0 kB\n", "{none}");
    // A run joining another's microVM is held to its own limits by its own cgroup there,
    // and swaps to that microVM's swap, which its members share: mapped by the first with
    // an allowance, each held to its own.
    let provider = run_in(&home, &image, &["-d", "--name", "provider"], &["sleep"]);
    assert_eq!(provider.status, Some(0), "{provider}");
    let joiner = |flags: &[&str], cmd: &[&str]| {
        let mut all = vec!["--network", "container:provider", "-m", "64m"];
        all.extend(flags);
        run(&all, cmd)
    };
    let held = joiner(&[], &["lines", "/sys/fs/cgroup/memory.max", ""]);
    assert_eq!(held.stdout, "67108864\n", "{held}");
    let killed = joiner(&["--memory-swap", "64m"], &["alloc", "96"]);
    assert_eq!(killed.status, Some(137), "{killed}");
    // Its end said in the daemon's log, as a run's is, as the daemon hears it, which may be
    // as its client has the exit: what its cgroup was allowed and held, none of it swap,
    // and what its kernel said of it.
    let deadline = Instant::now() + TIMEOUT;
    let (log, said) = loop {
        let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
        let said = log
            .lines()
            .rev()
            .find(|l| l.contains("a process ended for want of memory"))
            .map(str::to_string);
        if let Some(said) = said {
            break (log, said);
        }
        assert!(
            Instant::now() < deadline,
            "no kill said in the daemon's log:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        said.contains("memory.max 67108864")
            && said.contains("memory.swap.max 0")
            && said.contains("oom_kill 1")
            && said.contains("; the kernel: "),
        "{log}"
    );
    // Killed again (CI run 39245355471), the swap mapped and none of it used once the
    // joiner had gone: the daemon's log, shown as the test fails, says what its cgroup held
    // and was allowed as it was killed, and the kernel's report.
    for _ in 0..2 {
        let joined = joiner(&[], &["alloc", "96"]);
        // Killed within its swap, once on x86_64 CI (job 117491995198): what the microVM's
        // swap was then, mapped or not yet, and how much of it was free.
        let swap = |file: &str| {
            shards(&[
                "exec",
                "provider",
                "/bin/testguest",
                "fs",
                &format!("print:{file}"),
            ])
            .stdout
        };
        let failed = joined.status != Some(0);
        assert_eq!(
            (joined.status, joined.stdout.as_str()),
            (Some(0), "allocated 96\n"),
            "{joined}\n--- the microVM's /proc/swaps\n{}\n--- its /proc/meminfo\n{}",
            if failed {
                swap("/proc/swaps")
            } else {
                String::new()
            },
            if failed {
                swap("/proc/meminfo")
            } else {
                String::new()
            },
        );
    }
    assert_eq!(shards(&["rm", "-f", "provider"]).status, Some(0));
    // None where its limits give none: killed past its memory.
    assert_eq!(
        allowed(&["-m", "64m", "--memory-swap", "64m"]),
        ("0\n".to_string(), false)
    );
    let killed = run(&["-m", "64m", "--memory-swap", "64m"], &["alloc", "96"]);
    assert_eq!(killed.status, Some(137), "{killed}");
    assert_eq!(
        allowed(&["-m", "64m", "--memory-swap", "160m"]),
        ("100663296\n".to_string(), true)
    );
    let unlimited = run(&["-m", "64m", "--memory-swap", "-1"], &["alloc", "96"]);
    assert_eq!(unlimited.status, Some(0), "{unlimited}");
    // Its swappiness, its microVM's own.
    let swappy = run(
        &["--memory-swappiness", "10"],
        &["slurp", "/proc/sys/vm/swappiness"],
    );
    assert_eq!(swappy.stdout, "slurp /proc/sys/vm/swappiness: 10\n", "{swappy}");
    // The OOM killer kept off: slowed past its memory, never killed.
    let kept = run(
        &["-m", "64m", "--memory-swap", "64m", "--oom-kill-disable"],
        &["alloc", "72"],
    );
    assert_eq!(
        (kept.status, kept.stdout.as_str()),
        (Some(0), "allocated 72\n"),
        "{kept}"
    );
    for (flags, said) in [
        (
            &["--memory-swappiness", "101"][..],
            "--memory-swappiness 101: from 0 to 100",
        ),
        (
            &["--memory-swap", "64m"],
            "--memory-swap without --memory: swap is what a run may use past its memory",
        ),
        (
            &["-m", "64m", "--memory-swap", "32m"],
            "--memory-swap 33554432 is less than --memory 67108864",
        ),
        (
            &["-m", "1m"],
            "--memory 1048576: a run's memory is at least 6 MiB",
        ),
    ] {
        let r = run(flags, &["exit", "0"]);
        assert_ne!(r.status, Some(0), "{flags:?}: {r}");
        assert!(r.stderr.contains(said), "{flags:?}: {r}");
    }
    let made = run_in(
        &home,
        &image,
        &[
            "-d",
            "--name",
            "kept",
            "-m",
            "64m",
            "--memory-swappiness",
            "10",
            "--oom-kill-disable",
        ],
        &["sleep"],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.MemorySwappiness}} {{.HostConfig.OomKillDisable}} {{.HostConfig.MemorySwap}}",
        "kept",
    ]);
    assert_eq!(shown.stdout, "10 true 134217728\n", "{shown}");
    // Its swap file is its microVM's alone once the microVM holds it: nothing of it is left
    // in the container's directory.
    let id = shards(&["inspect", "-f", "{{.Id}}", "kept"])
        .stdout
        .trim()
        .to_string();
    let file = home.join("containers").join(&id).join("swap");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while file.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!file.exists(), "{}", file.display());
    assert_eq!(shards(&["rm", "-f", "kept"]).status, Some(0));
}

/// A run's IPC, hostname, cgroups, umask and writable layer as its microVM has them (D156).
/// A run has an IPC namespace and a /dev/shm of its own, which its execs share, as large as
/// the microVM's: half the memory it may have, which `inspect` says, and which takes what a
/// browser's renderers write there (Docker's 64 MiB refused 128 MiB); `--shm-size` sizes
/// it, `--ipc none` takes it away, and `--ipc host` gives the microVM's. A run joining another's microVM shares that
/// one's (`--ipc container:NAME`) only where it said so (`--ipc shareable`), a joiner's
/// too; `--ipc host` runs share the microVM's. `--uts host` names the run by its host,
/// `--cgroupns host` shows it the microVM's cgroup tree, `--umask` is its command's and its
/// execs', and `--storage-opt size=` holds its writable layer to that size. Each refusal
/// in shards' words; `inspect` says each as given.
#[test]
fn a_microvm_takes_its_ipc_hostname_cgroups_umask_and_writable_layer() {
    let Some((home, image)) = home("containers-microvm-ipc") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let ipc = |flags: &[&str], key: &str| {
        let mut all = vec!["--rm"];
        all.extend(flags);
        let r = run_in(&home, &image, &all, &["ipc", key]);
        assert_eq!(r.status, Some(0), "{flags:?}: {r}");
        r.stdout
    };
    let shm = |said: &str| {
        said.lines()
            .find_map(|l| l.strip_prefix("shm "))
            .unwrap_or_default()
            .to_string()
    };
    // Its own /dev/shm, the microVM's size, and the sizes asked of it.
    let host_shm: u64 = shm(&ipc(&["--ipc", "host"], "")).parse().unwrap();
    assert!(host_shm > 128 << 20, "the microVM's /dev/shm: {host_shm}");
    assert_eq!(shm(&ipc(&[], "")), host_shm.to_string());
    assert_eq!(shm(&ipc(&["--shm-size", "16m"], "")), (16u64 << 20).to_string());
    assert_eq!(shm(&ipc(&["--ipc", "none"], "")), "none");
    // A tmpfs of the run's own at /dev/shm goes over the one it was given, as its size
    // says, sized before anything was mounted: never resized itself.
    let own = ["--shm-size", "16m", "--tmpfs", "/dev/shm:size=8m"];
    assert_eq!(shm(&ipc(&own[2..], "")), (8u64 << 20).to_string());
    assert_eq!(shm(&ipc(&own, "")), (8u64 << 20).to_string());
    let wrote = run_in(&home, &image, &["--rm"], &["big", "/dev/shm/renderer", "128"]);
    assert_eq!(wrote.status, Some(0), "128 MiB in its /dev/shm: {wrote}");
    let made = run_in(&home, &image, &["-d", "--name", "sized"], &["sleep", "600"]);
    assert_eq!(made.status, Some(0), "{made}");
    let said = shards(&["inspect", "-f", "{{.HostConfig.ShmSize}}", "sized"]);
    assert_eq!(said.stdout.trim(), host_shm.to_string(), "{said}");
    assert_eq!(shards(&["rm", "-f", "sized"]).status, Some(0));
    for (flags, said) in [
        (&["--ipc", "bogus"][..], "--ipc bogus: give private"),
        (
            &["--ipc", "none", "--shm-size", "16m"],
            "--shm-size sizes a run's own /dev/shm, and --ipc none gives it none",
        ),
        (
            &["--ipc", "host", "--shm-size", "16m"],
            "--shm-size sizes a run's own /dev/shm, and it is the microVM's (--ipc host)",
        ),
        (
            &["--uts", "host", "--hostname", "h"],
            "--hostname and --uts host each name the microVM's host: give one",
        ),
        (&["--uts", "other"], "--uts other: give host"),
        (&["--cgroupns", "other"], "--cgroupns other: give private"),
        (&["--userns", "other"], "--userns other: give host"),
        (&["--umask", "7777"], "--umask 07777: a umask is at most 0777"),
        (
            &["--storage-opt", "inodes=10"],
            "--storage-opt inodes=10: a run's writable layer takes size alone",
        ),
        (
            &["--storage-opt", "size=0"],
            "--storage-opt size=0: a size is a number of bytes above 0",
        ),
        (
            &["--storage-opt", "size=20t"],
            "--storage-opt size=21990232555520: an ext4 file system of 21990232555520 bytes: at most 16 TiB",
        ),
    ] {
        let mut all = vec!["--rm"];
        all.extend(flags);
        let r = run_in(&home, &image, &all, &["exit", "0"]);
        assert_ne!(r.status, Some(0), "{flags:?}: {r}");
        assert!(r.stderr.contains(said), "{flags:?}: {r}");
    }
    // Its execs in its IPC namespace and /dev/shm, with its umask.
    let held = |name: &str, flags: &[&str], key: &str| {
        let mut all = vec!["-d", "--name", name];
        all.extend(flags);
        let r = run_in(&home, &image, &all, &["ipc-hold", key]);
        assert_eq!(r.status, Some(0), "{name}: {r}");
        let deadline = Instant::now() + TIMEOUT;
        while !shards(&["logs", name]).stdout.contains("held") {
            assert!(
                Instant::now() < deadline,
                "{name} never held its segment: {}",
                shards(&["logs", name])
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    held("owner", &["--ipc", "shareable", "--umask", "027"], "42");
    let exec = shards(&["exec", "owner", "/bin/testguest", "ipc", "42"]);
    assert!(exec.stdout.contains("sysv 42 yes\nfile 42 yes\n"), "{exec}");
    let mask = shards(&["exec", "owner", "/bin/testguest", "umask"]);
    assert_eq!(mask.stdout, "umask 0027\n", "{mask}");
    let mask = run_in(&home, &image, &["--rm", "--umask", "077"], &["umask"]);
    assert_eq!(mask.stdout, "umask 0077\n", "{mask}");
    // A joiner shares a shareable run's IPC where it asks to, and has its own otherwise.
    let shared = ipc(
        &["--network", "container:owner", "--ipc", "container:owner"],
        "42",
    );
    assert!(shared.contains("sysv 42 yes\nfile 42 yes\n"), "{shared}");
    let own = ipc(&["--network", "container:owner"], "42");
    assert!(
        own.contains(&format!("shm {host_shm}\nsysv 42 errno 2\nfile 42 errno 2\n")),
        "{own}"
    );
    // inspect says the /dev/shm each sees: a joiner's, of the microVM it lives in, whatever
    // memory limit it has of its own; none where it has none.
    let sized = |name: &str, flags: &[&str]| {
        let mut all = vec!["-d", "--name", name];
        all.extend(flags);
        let r = run_in(&home, &image, &all, &["sleep"]);
        assert_eq!(r.status, Some(0), "{name}: {r}");
        let said = shards(&["inspect", "-f", "{{.HostConfig.ShmSize}}", name]);
        assert_eq!(shards(&["rm", "-f", name]).status, Some(0));
        said.stdout.trim().to_string()
    };
    assert_eq!(
        sized("joined", &["--network", "container:owner", "-m", "256m"]),
        host_shm.to_string()
    );
    assert_eq!(sized("unshared", &["--ipc", "none"]), "0");
    let microvms = ipc(&["--network", "container:owner", "--ipc", "host"], "42");
    assert!(
        microvms.contains("sysv 42 errno 2\nfile 42 errno 2\n"),
        "{microvms}"
    );
    // A joiner's, shared with another joiner.
    held(
        "first",
        &["--network", "container:owner", "--ipc", "shareable"],
        "45",
    );
    let second = ipc(
        &["--network", "container:owner", "--ipc", "container:first"],
        "45",
    );
    assert!(second.contains("sysv 45 yes\nfile 45 yes\n"), "{second}");
    // A joiner's writable layer and cgroup namespace, as a run's.
    let joined_layer = run_in(
        &home,
        &image,
        &["--rm", "--network", "container:owner", "--storage-opt", "size=4m"],
        &["big", "/tmp/big", "8"],
    );
    assert_ne!(joined_layer.status, Some(0), "{joined_layer}");
    assert!(
        joined_layer.stderr.contains("No space left on device"),
        "{joined_layer}"
    );
    // Held in its microVM's memory, a joiner's layer is no larger than that memory.
    let joined_past = run_in(
        &home,
        &image,
        &[
            "--rm",
            "--network",
            "container:owner",
            "--memory",
            "512m",
            "--storage-opt",
            "size=1g",
        ],
        &["exit", "0"],
    );
    assert!(
        joined_past.stderr.contains(
            "--storage-opt size=1073741824: a joiner's writable layer is held in its microVM's memory, and 1073741824 bytes is more than its memory (--memory 536870912)"
        ),
        "{joined_past}"
    );
    let joined_tree = run_in(
        &home,
        &image,
        &["--rm", "--network", "container:owner", "--cgroupns", "host"],
        &["slurp", "/proc/self/cgroup"],
    );
    assert!(
        joined_tree.stdout.starts_with("slurp /proc/self/cgroup: 0::/")
            && joined_tree.stdout != "slurp /proc/self/cgroup: 0::/\n",
        "{joined_tree}"
    );
    // The microVM's, shared by the runs that ask for it.
    held("hosts", &["--ipc", "host"], "44");
    let joined = ipc(&["--network", "container:hosts", "--ipc", "host"], "44");
    assert!(joined.contains("sysv 44 yes\nfile 44 yes\n"), "{joined}");
    // A run that keeps its own, and one sharing without joining.
    held("private", &[], "43");
    for (flags, said) in [
        (
            &["--network", "container:private", "--ipc", "container:private"][..],
            "--ipc container:private: private keeps its IPC its own (--ipc private); run it with --ipc shareable to share it",
        ),
        (
            &["--ipc", "container:owner"],
            "--ipc container:owner: a run shares another's IPC inside that run's microVM, which it joins with --network container:owner",
        ),
    ] {
        let mut all = vec!["--rm"];
        all.extend(flags);
        let r = run_in(&home, &image, &all, &["exit", "0"]);
        assert_ne!(r.status, Some(0), "{flags:?}: {r}");
        assert!(r.stderr.contains(said), "{flags:?}: {r}");
    }
    // Its host's hostname; the microVM's cgroup tree.
    let host = common::host_name();
    let host = host.get(..host.len().min(64)).unwrap().to_string();
    let named = run_in(
        &home,
        &image,
        &["--rm", "--uts", "host"],
        &["slurp", "/proc/sys/kernel/hostname"],
    );
    assert_eq!(
        named.stdout,
        format!("slurp /proc/sys/kernel/hostname: {host}\n"),
        "{named}"
    );
    let own_tree = run_in(&home, &image, &["--rm"], &["slurp", "/proc/self/cgroup"]);
    assert_eq!(own_tree.stdout, "slurp /proc/self/cgroup: 0::/\n", "{own_tree}");
    let whole = run_in(
        &home,
        &image,
        &["--rm", "--cgroupns", "host"],
        &["slurp", "/proc/self/cgroup"],
    );
    assert!(
        whole.stdout.starts_with("slurp /proc/self/cgroup: 0::/") && whole.stdout != own_tree.stdout,
        "{whole}"
    );
    // Its writable layer, held to its size.
    let layer = |size: &str, mib: &str| {
        run_in(
            &home,
            &image,
            &["--rm", "--storage-opt", &format!("size={size}")],
            &["big", "/tmp/big", mib],
        )
    };
    let fits = layer("16m", "4");
    assert_eq!(fits.status, Some(0), "{fits}");
    let past = layer("4m", "8");
    assert_ne!(past.status, Some(0), "{past}");
    assert!(past.stderr.contains("No space left on device"), "{past}");
    // Its host's disk, not its memory (D190): a run held to 64 MiB of memory writes 256 MiB
    // there, where a layer in its memory would have ended it.
    let beyond = run_in(
        &home,
        &image,
        &["--rm", "--memory", "64m"],
        &["big", "/tmp/big", "256"],
    );
    assert_eq!(beyond.status, Some(0), "{beyond}");
    // As given, in inspect.
    let made = run_in(
        &home,
        &image,
        &[
            "-d",
            "--name",
            "described",
            "--ipc",
            "none",
            "--uts",
            "host",
            "--userns",
            "host",
            "--cgroupns",
            "host",
            "--umask",
            "022",
            "--storage-opt",
            "size=64m",
        ],
        &["sleep"],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let described = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.IpcMode}} {{.HostConfig.UTSMode}} {{.HostConfig.UsernsMode}} {{.HostConfig.CgroupnsMode}} {{json .HostConfig.StorageOpt}} {{.HostConfig.Umask}}",
        "described",
    ]);
    assert_eq!(
        described.stdout, "none host host host {\"size\":\"64m\"} 18\n",
        "{described}"
    );
    let plain = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.IpcMode}} {{.HostConfig.CgroupnsMode}}",
        "private",
    ]);
    assert_eq!(plain.stdout, "private private\n", "{plain}");
    for name in ["owner", "first", "hosts", "private", "described"] {
        let removed = shards(&["rm", "-f", name]);
        assert_eq!(removed.status, Some(0), "{name}: {removed}");
    }
}

/// `-a` (docker/cli opts.go): its client gets the streams it attaches, its log every
/// stream; with `-d`, refused in shards' words; `create`'s, as `inspect` says them.
#[test]
fn a_run_attaches_the_streams_it_asks_for() {
    let Some((home, image)) = home("containers-attach") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    // What the command said, the timing line the tests' client adds left out.
    let said = |r: &Run| {
        let err: String = r
            .stderr
            .lines()
            .filter(|l| !l.starts_with("shards-timing "))
            .map(|l| format!("{l}\n"))
            .collect();
        (r.status, r.stdout.clone(), err)
    };
    let both = run_in(&home, &image, &["--name", "both"], &["streams", "out", "err"]);
    assert_eq!(said(&both), (Some(0), "out\n".into(), "err\n".into()), "{both}");
    let out = run_in(
        &home,
        &image,
        &["--name", "out", "-a", "stdout"],
        &["streams", "out", "err"],
    );
    assert_eq!(said(&out), (Some(0), "out\n".into(), String::new()), "{out}");
    let err = run_in(
        &home,
        &image,
        &["--name", "err", "-a", "STDERR"],
        &["streams", "out", "err"],
    );
    assert_eq!(said(&err), (Some(0), String::new(), "err\n".into()), "{err}");
    // Its log holds every stream, whichever were attached.
    for name in ["out", "err"] {
        let logged = shards(&["logs", name]);
        assert_eq!(
            said(&logged),
            (Some(0), "out\n".into(), "err\n".into()),
            "{name}: {logged}"
        );
    }
    // Stdin alone: nothing shown, and its status the client's.
    let quiet = run_in(&home, &image, &["-a", "stdin"], &["exit", "3"]);
    assert_eq!(said(&quiet), (Some(3), String::new(), String::new()), "{quiet}");
    let detached = run_in(&home, &image, &["-d", "-a", "stdout"], &["exit", "0"]);
    assert_eq!(detached.status, Some(125), "{detached}");
    assert!(
        detached.stderr.starts_with(
            "shards: --attach and --detach: a detached run's streams go to its log alone: give one\n"
        ),
        "{detached}"
    );
    let made = shards(&[
        "create", "--pull", "never", "--name", "made", "-a", "stdout", &image,
    ]);
    assert_eq!(made.status, Some(0), "{made}");
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.Config.AttachStdin}} {{.Config.AttachStdout}} {{.Config.AttachStderr}}",
        "made",
    ]);
    assert_eq!(shown.stdout, "false true false\n", "{shown}");
    // Without `-a`, stdout and stderr, as docker/cli's create keeps them.
    let plain = shards(&["create", "--pull", "never", "--name", "plain", &image]);
    assert_eq!(plain.status, Some(0), "{plain}");
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.Config.AttachStdin}} {{.Config.AttachStdout}} {{.Config.AttachStderr}}",
        "plain",
    ]);
    assert_eq!(shown.stdout, "false true true\n", "{shown}");
}

/// A run's log compressed and queued as its options say (D162). Its rotated segments are
/// compressed, seekably, unless its driver is json-file, which compresses them only when
/// asked (as Docker's does, though Docker refuses json-file's `compress` without a
/// `max-size`). `logs` reads them back whole and from their middle (`--tail`). A
/// non-blocking log never holds up its run: what it keeps and what it drops add up to the
/// run's output, and `logs` says what it dropped. Its refusals are in shards' words.
#[test]
fn a_runs_log_is_compressed_and_queued_as_its_options_say() {
    let Some((home, image)) = home("containers-log-packing") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let dir_of = |name: &str| {
        let id = shards(&["inspect", "-f", "{{.Id}}", name]).stdout;
        home.join("containers").join(id.trim())
    };
    let names = |dir: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("log") && !n.ends_with(".idx"))
            .collect();
        names.sort();
        names
    };
    // Every segment but the newest compressed, by the packer's thread, as they rotate.
    let packed = |dir: &Path| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let names = names(dir);
            let plain = names.iter().filter(|n| !n.ends_with(".zst")).count();
            if plain == 1 || Instant::now() > deadline {
                return names;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // 30,000 lines through segments of 64,000 bytes, four kept.
    let rotating = run_in(
        &home,
        &image,
        &[
            "--name",
            "rotating",
            "--log-opt",
            "max-size=64k",
            "--log-opt",
            "max-file=4",
        ],
        &["count", "30000"],
    );
    assert_eq!(rotating.status, Some(0), "{rotating}");
    let kept = packed(&dir_of("rotating"));
    assert_eq!(kept.len(), 4, "{kept:?}");
    assert_eq!(kept.iter().filter(|n| n.ends_with(".zst")).count(), 3, "{kept:?}");
    let logged = shards(&["logs", "rotating"]);
    assert_eq!((logged.status, logged.stderr.as_str()), (Some(0), ""), "{logged}");
    // What its segments hold, compressed or not, past their records' heads: the output's
    // newest, all of it, each segment filled to its size but the newest.
    let dir = dir_of("rotating");
    let mut seqs: Vec<u64> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_prefix("log.")?.strip_suffix(".idx")?.parse().ok()
        })
        .collect();
    seqs.sort_unstable();
    let mut payload = 0;
    for (i, seq) in seqs.iter().enumerate() {
        let log = segment_log(&dir, *seq).len() as u64;
        let records = std::fs::metadata(dir.join(format!("log.{seq}.idx")))
            .unwrap()
            .len()
            / 8;
        // Filled until less is left than a record's head and a byte.
        if i + 1 < seqs.len() {
            assert!(
                (64_000 - 13..=64_000).contains(&log),
                "segment {seq}: {log} bytes"
            );
        }
        payload += log - 13 * records;
    }
    assert_eq!(logged.stdout.len() as u64, payload);
    assert!(
        rotating.stdout.ends_with(&logged.stdout),
        "{} bytes logged: not the output's newest",
        logged.stdout.len()
    );
    // Its last lines, from the middle of a compressed segment.
    let tail = shards(&["logs", "--tail", "12000", "rotating"]);
    let want: Vec<&str> = rotating.stdout.lines().skip(30000 - 12000).collect();
    assert_eq!(tail.stdout.lines().collect::<Vec<_>>(), want);
    // json-file keeps them as written, unless asked to compress them.
    for (compress, zst) in [(None, 0), (Some("compress=true"), 3)] {
        let name = format!("json-{}", zst);
        let mut flags = vec![
            "--name",
            name.as_str(),
            "--log-driver",
            "json-file",
            "--log-opt",
            "max-size=64k",
            "--log-opt",
            "max-file=4",
        ];
        if let Some(c) = compress {
            flags.extend(["--log-opt", c]);
        }
        let r = run_in(&home, &image, &flags, &["count", "30000"]);
        assert_eq!(r.status, Some(0), "{r}");
        let kept = if zst > 0 {
            packed(&dir_of(&name))
        } else {
            names(&dir_of(&name))
        };
        assert_eq!(
            kept.iter().filter(|n| n.ends_with(".zst")).count(),
            zst,
            "{compress:?}: {kept:?}"
        );
        let logged = shards(&["logs", &name]);
        assert!(r.stdout.ends_with(&logged.stdout), "{compress:?}");
    }
    // Non-blocking, a queue of none: what the log kept and what it dropped are the output.
    let queued = run_in(
        &home,
        &image,
        &[
            "--name",
            "queued",
            "--log-opt",
            "mode=non-blocking",
            "--log-opt",
            "max-buffer-size=0",
            "--log-opt",
            "max-size=10m",
        ],
        &["count", "30000"],
    );
    assert_eq!(queued.status, Some(0), "{queued}");
    let logged = shards(&["logs", "queued"]);
    assert_eq!(logged.status, Some(0), "{logged}");
    let dropped: usize = match logged.stderr.as_str() {
        "" => 0,
        said => said
            .strip_prefix("shards: ")
            .and_then(|s| s.split_once(" bytes of queued's output were dropped, as they came while its log's queue was full (--log-opt mode=non-blocking)\n"))
            .and_then(|(n, rest)| rest.is_empty().then(|| n.parse().ok()).flatten())
            .unwrap_or_else(|| panic!("{logged}")),
    };
    assert_eq!(
        logged.stdout.len() + dropped,
        queued.stdout.len(),
        "{dropped} dropped"
    );
    // A queue it never fills: all of it, and nothing said.
    let roomy = run_in(
        &home,
        &image,
        &[
            "--name",
            "roomy",
            "--log-opt",
            "mode=non-blocking",
            "--log-opt",
            "max-buffer-size=4m",
            "--log-opt",
            "max-size=10m",
        ],
        &["count", "30000"],
    );
    assert_eq!(roomy.status, Some(0), "{roomy}");
    let logged = shards(&["logs", "roomy"]);
    assert_eq!((logged.status, logged.stderr.as_str()), (Some(0), ""), "{logged}");
    assert_eq!(logged.stdout, roomy.stdout);
    for (flags, said) in [
        (
            &["--log-opt", "compress=true", "--log-opt", "max-file=1"][..],
            "shards: --log-opt compress=true: its log is one file (max-file 1), never rotated",
        ),
        (
            &[
                "--log-opt",
                "mode=non-blocking",
                "--log-opt",
                "max-buffer-size=lots",
            ],
            "shards: --log-opt max-buffer-size=lots: a size",
        ),
        (
            &["--log-opt", "max-buffer-size=1m"],
            "shards: --log-opt max-buffer-size: a buffer is non-blocking mode's",
        ),
    ] {
        let r = run_in(&home, &image, flags, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{flags:?}: {r}");
        assert!(r.stderr.starts_with(said), "{flags:?}: {r}");
    }
}

/// `listener`'s first connection, within [`TIMEOUT`]: none where none comes, which a test
/// then fails on, not waits for ever.
fn accept_within(listener: &std::net::TcpListener) -> Option<std::net::TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(false).unwrap();
                return Some(conn);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// What a syslog server reads on `listener`'s first connection: its lines until `lines`
/// have come, or all it gets until the connection ends; none where none comes.
fn syslog_lines(listener: std::net::TcpListener, lines: usize) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let Some(conn) = accept_within(&listener) else {
            return Vec::new();
        };
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut read = BufReader::new(conn);
        let mut got = Vec::new();
        while got.len() < lines {
            let mut line = String::new();
            if read.read_line(&mut line).unwrap() == 0 {
                break;
            }
            got.push(line);
        }
        got.sort();
        got
    })
}

/// A port nothing listens on now, which a test may listen on later.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A run's log sent to syslog (D163): by the daemon, from shards' store, so that its run
/// waits for no server. Each line is a message as Docker's syslog driver sends it:
/// - over UDP, TCP, TLS (octet-counted) and a unix datagram socket;
/// - its severity info for stdout and err for stderr, of the facility asked for;
/// - its tag as asked, else the container's short ID;
/// - stamped with when its output came (Docker stamps when it sends);
/// - naming this host (Docker names its socket's address and port);
/// - a line longer than 16 KiB in parts.
///
/// One sent to a server that comes up only after its run gets there, and so does one
/// whose daemon restarted meanwhile: Docker's would not start without its server, and
/// would lose what was not yet sent. `logs` reads the local copy, and refuses where
/// `cache-disabled` keeps it. The refusals are in shards' words.
#[test]
fn a_runs_log_is_sent_to_syslog_as_its_options_say() {
    let Some((home, image)) = home("containers-syslog") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let syslog = |name: &str, opts: &[&str], command: &[&str]| {
        let mut flags = vec!["--name", name, "--log-driver", "syslog"];
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        let r = run_in(&home, &image, &flags, command);
        assert_eq!(r.status, Some(0), "{name}: {r}");
        r
    };
    // UDP, srslog's Unix format and its facility, daemon: info 30, err 27.
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_read_timeout(Some(TIMEOUT)).unwrap();
    let datagram = |s: &std::net::UdpSocket| {
        let mut b = vec![0u8; 1 << 16];
        let n = s.recv(&mut b).unwrap();
        String::from_utf8(b[..n].to_vec()).unwrap()
    };
    syslog(
        "udp",
        &[
            &format!("syslog-address=udp://{}", udp.local_addr().unwrap()),
            "tag={{.Name}}",
        ],
        &["streams", "out", "err"],
    );
    let mut got = [datagram(&udp), datagram(&udp)];
    got.sort();
    let unix =
        regex::Regex::new(r"^<(\d+)>[A-Z][a-z]{2} [ \d]\d \d\d:\d\d:\d\d udp\[\d+\]: (\w+)\n$").unwrap();
    let said: Vec<(String, String)> = got
        .iter()
        .map(|m| {
            let c = unix.captures(m).unwrap_or_else(|| panic!("{m:?}"));
            (c[1].to_string(), c[2].to_string())
        })
        .collect();
    assert_eq!(said, [("27".into(), "err".into()), ("30".into(), "out".into())]);
    assert_eq!(shards(&["logs", "udp"]).stdout, "out\n");
    assert_eq!(shards(&["inspect", "-f", "{{.LogPath}}", "udp"]).stdout, "\n");

    // TCP, RFC 5424 to the microsecond, local0: its tag the short ID, its host this one's,
    // its time its output's.
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("syslog-address=tcp://{}", tcp.local_addr().unwrap());
    let reading = syslog_lines(tcp, 2);
    syslog(
        "tcp",
        &[&address, "syslog-format=rfc5424micro", "syslog-facility=local0"],
        &["streams", "out", "err"],
    );
    let got = reading.join().unwrap();
    let id = shards(&["inspect", "-f", "{{.Id}}", "tcp"]).stdout;
    let short = id.get(..12).unwrap();
    let micro = regex::Regex::new(r"^<(\d+)>1 (\S+) (\S+) (\S+) (\d+) (\S+) - (\w+)\n$").unwrap();
    let stamped = shards(&["logs", "-t", "tcp"]);
    for m in &got {
        let c = micro.captures(m).unwrap_or_else(|| panic!("{m:?}"));
        let (pri, time, host, app, msgid, line) = (&c[1], &c[2], &c[3], &c[4], &c[6], &c[7]);
        assert_eq!((app, msgid), (short, short), "{m:?}");
        assert!(
            !host.is_empty() && !host.contains(':'),
            "a host's name, not an address: {m:?}"
        );
        let pri: u32 = pri.parse().unwrap();
        assert_eq!(
            (pri >> 3, pri & 7),
            (16, if line == "err" { 3 } else { 6 }),
            "{m:?}"
        );
        // Its time is its output's, as `logs -t` says it, to the microsecond.
        let rfc3339 = &time::format_description::well_known::Rfc3339;
        let sent = time::OffsetDateTime::parse(time, rfc3339)
            .unwrap()
            .unix_timestamp_nanos();
        let kept = [&stamped.stdout, &stamped.stderr]
            .iter()
            .flat_map(|s| s.lines())
            .find_map(|l| l.strip_suffix(&format!(" {line}")))
            .unwrap_or_else(|| panic!("{stamped}"));
        let kept = time::OffsetDateTime::parse(kept, rfc3339)
            .unwrap()
            .unix_timestamp_nanos();
        assert_eq!(sent, kept / 1000 * 1000, "{m:?}: {stamped}");
    }

    // TLS to a server whose CA it is given, RFC 5424: octet-counted (RFC 5425).
    let now = time::OffsetDateTime::now_utc();
    let mut ca = rcgen::CertificateParams::new(Vec::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    // Named apart: Windows' chain engine takes a certificate whose subject is its issuer's
    // for one that signed itself, and checks its signature by its own key
    // (TRUST_E_CERT_SIGNATURE), where rcgen names both alike unless told.
    ca.distinguished_name = rcgen::DistinguishedName::new();
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "shards test CA");
    ca.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    ca.not_before = now - time::Duration::hours(1);
    ca.not_after = now + time::Duration::days(1);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = rcgen::CertifiedIssuer::self_signed(ca, ca_key).unwrap();
    let mut leaf = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    leaf.distinguished_name = rcgen::DistinguishedName::new();
    leaf.distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    leaf.not_before = now - time::Duration::hours(1);
    leaf.not_after = now + time::Duration::days(1);
    leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&leaf_key, &ca).unwrap();
    let ca_file = home.join("syslog-ca.pem");
    std::fs::write(&ca_file, ca.pem()).unwrap();
    let server = std::sync::Arc::new(
        rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                leaf_key.serialize_der(),
            )),
        )
        .unwrap(),
    );
    let tls = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tls.local_addr().unwrap().port();
    let reading = std::thread::spawn(move || {
        let Some(conn) = accept_within(&tls) else {
            return (String::new(), Err(std::io::ErrorKind::NotConnected));
        };
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut stream = rustls::StreamOwned::new(rustls::ServerConnection::new(server).unwrap(), conn);
        let mut got = Vec::new();
        // Its frames, each its length, a space, and its message; then its end: Ok at TLS's
        // close_notify, an error at a reset or an end without it.
        let ended = std::io::Read::read_to_end(&mut stream, &mut got)
            .map(|_| ())
            .map_err(|e| e.kind());
        (String::from_utf8(got).unwrap(), ended)
    });
    syslog(
        "tls",
        &[
            &format!("syslog-address=tcp+tls://localhost:{port}"),
            &format!("syslog-tls-ca-cert={}", ca_file.display()),
            "syslog-format=rfc5424",
            "tag=tls",
        ],
        &["streams", "out", "err"],
    );
    // Its run's end ends its connection only once the server has all of it, cleanly: a
    // socket closed with the server's session tickets unread resets the connection, and
    // took the run's last line with it (D163's CI, PM M233).
    let (mut framed, ended) = reading.join().unwrap();
    assert_eq!(ended, Ok(()), "{framed:?}");
    let mut messages = Vec::new();
    while !framed.is_empty() {
        let (len, rest) = framed.split_once(' ').unwrap_or_else(|| panic!("{framed:?}"));
        let len: usize = len.parse().unwrap_or_else(|_| panic!("{framed:?}"));
        messages.push(rest.get(..len).unwrap().to_string());
        framed = rest.get(len..).unwrap().to_string();
    }
    messages.sort();
    let tls_said = regex::Regex::new(r"^<(\d+)>1 \S+ \S+ tls \d+ tls - (\w+)\n$").unwrap();
    let said: Vec<(String, String)> = messages
        .iter()
        .map(|m| {
            let c = tls_said.captures(m).unwrap_or_else(|| panic!("{m:?}"));
            (c[1].to_string(), c[2].to_string())
        })
        .collect();
    assert_eq!(said, [("27".into(), "err".into()), ("30".into(), "out".into())]);

    // A unix datagram socket; a line of 40,000 bytes in parts of 16 KiB. Windows has none,
    // and refuses one's address as a run is made: its parts go on a unix stream there.
    let sockets = TempDir::new("sl");
    #[cfg(windows)]
    {
        let gram = sockets.join("g");
        std::fs::write(&gram, b"").unwrap();
        let address = format!("syslog-address=unixgram:///{}", gram.display());
        let r = run_in(
            &home,
            &image,
            &["--log-driver", "syslog", "--log-opt", &address],
            &["exit", "0"],
        );
        assert_eq!(r.status, Some(125), "{r}");
        assert!(r.stderr.contains("Windows has no unix datagram sockets"), "{r}");
        let path = sockets.join("s");
        shards_vmm::platform::winsock().unwrap();
        let unix = shards_vmm::platform::UnixListener::bind(&path).unwrap();
        let reading = std::thread::spawn(move || {
            let (conn, ()) = unix.accept().unwrap();
            conn.set_read_timeout(Some(TIMEOUT)).unwrap();
            let mut read = BufReader::new(conn);
            (0..3)
                .map(|_| {
                    let mut line = String::new();
                    read.read_line(&mut line).unwrap();
                    line
                })
                .collect::<Vec<String>>()
        });
        syslog(
            "gram",
            &[&format!("syslog-address=unix:///{}", path.display()), "tag=g"],
            &["wide", "40000"],
        );
        let parts: Vec<usize> = reading
            .join()
            .unwrap()
            .iter()
            .map(|m| {
                let body = m.split_once("]: ").unwrap().1.trim_end_matches('\n');
                assert!(body.bytes().all(|b| b == b'x'), "{m:?}");
                body.len()
            })
            .collect();
        assert_eq!(parts, [16384, 16384, 7232]);
    }
    #[cfg(unix)]
    let gram = sockets.join("g");
    #[cfg(unix)]
    let unixgram = std::os::unix::net::UnixDatagram::bind(&gram).unwrap();
    #[cfg(unix)]
    {
        unixgram.set_read_timeout(Some(TIMEOUT)).unwrap();
        // Room for a part whole: macOS's receive buffer takes no datagram longer (PM M232).
        let room: libc::c_int = 1 << 16;
        // SAFETY: setsockopt(2) of an int from `room` on a socket this test holds.
        let set = unsafe {
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&unixgram),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&raw const room).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(set, 0);
        syslog(
            "gram",
            &[&format!("syslog-address=unixgram://{}", gram.display()), "tag=g"],
            &["wide", "40000"],
        );
        let mut parts = Vec::new();
        for _ in 0..3 {
            let mut b = vec![0u8; 1 << 16];
            let n = unixgram.recv(&mut b).unwrap();
            let m = String::from_utf8(b[..n].to_vec()).unwrap();
            let body = m.split_once("]: ").unwrap().1.trim_end_matches('\n');
            assert!(body.bytes().all(|b| b == b'x'), "{m:?}");
            parts.push(body.len());
        }
        assert_eq!(parts, [16384, 16384, 7232]);
    }

    // A server that comes up only after the run: its log waits in the store, and goes.
    let port = free_port();
    syslog(
        "later",
        &[&format!("syslog-address=tcp://127.0.0.1:{port}"), "tag=later"],
        &["streams", "out", "err"],
    );
    let reading = syslog_lines(std::net::TcpListener::bind(("127.0.0.1", port)).unwrap(), 2);
    let got = reading.join().unwrap();
    assert!(
        got.len() == 2 && got[0].ends_with(": err\n") && got[1].ends_with(": out\n"),
        "{got:?}"
    );

    // One whose daemon stops before its server comes up: the next daemon sends it.
    let port = free_port();
    syslog(
        "restarted",
        &[&format!("syslog-address=tcp://127.0.0.1:{port}"), "tag=restarted"],
        &["streams", "out", "err"],
    );
    let stopped = without_vms(&home, &["daemon", "stop"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let reading = syslog_lines(std::net::TcpListener::bind(("127.0.0.1", port)).unwrap(), 2);
    assert_eq!(shards(&["ps", "-a", "-q"]).status, Some(0));
    let got = reading.join().unwrap();
    assert!(
        got.len() == 2 && got[0].ends_with(": err\n") && got[1].ends_with(": out\n"),
        "{got:?}"
    );

    // Started again: its next run's lines go too, as the first's have.
    syslog(
        "again",
        &[
            &format!("syslog-address=udp://{}", udp.local_addr().unwrap()),
            "tag=again",
        ],
        &["streams", "one", "two"],
    );
    let mut first = [datagram(&udp), datagram(&udp)];
    first.sort();
    let started = shards(&["start", "-a", "again"]);
    assert_eq!(started.status, Some(0), "{started}");
    let mut next = [datagram(&udp), datagram(&udp)];
    next.sort();
    // Sorted, stderr's err (27) before stdout's info (30).
    for got in [first, next] {
        assert!(
            got[0].ends_with(": two\n") && got[1].ends_with(": one\n"),
            "{got:?}"
        );
    }

    // Its local copy kept from `logs`.
    syslog(
        "uncached",
        &[
            &format!("syslog-address=udp://{}", udp.local_addr().unwrap()),
            "cache-disabled=true",
        ],
        &["streams", "out", "err"],
    );
    let mut got = [datagram(&udp), datagram(&udp)];
    got.sort();
    assert!(
        got[0].ends_with(": err\n") && got[1].ends_with(": out\n"),
        "{got:?}"
    );
    let refused = shards(&["logs", "uncached"]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (
            Some(1),
            "shards: uncached keeps no copy of its log: it was run with --log-opt cache-disabled=true, and its log goes to syslog alone\n"
        ),
        "{refused}"
    );

    for (opts, said) in [
        (
            &["syslog-tls-skip-verify=true"][..],
            "shards: --log-opt syslog-tls-skip-verify: shards sends a log only to a syslog server whose certificate it verifies",
        ),
        (
            &["syslog-address=http://h"],
            "shards: --log-opt syslog-address=http://h: the scheme http",
        ),
        (
            &["syslog-facility=24"],
            "shards: --log-opt syslog-facility=24: a facility's name",
        ),
        (
            &["max-size=1m"],
            "shards: --log-opt max-size: --log-driver syslog takes syslog-address",
        ),
        (
            &[
                "syslog-address=tcp+tls://localhost:6514",
                "syslog-tls-ca-cert=/nonexistent/ca.pem",
            ],
            "shards: --log-opt syslog-tls-ca-cert=/nonexistent/ca.pem: ",
        ),
    ] {
        let mut flags = vec!["--log-driver", "syslog"];
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        let r = run_in(&home, &image, &flags, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{opts:?}: {r}");
        assert!(r.stderr.starts_with(said), "{opts:?}: {r}");
    }
}

/// The GELF messages of `datagrams`, as a GELF server reads them: chunks gathered by
/// their message's ID, then gzip's, zlib's or as they are; each message's JSON.
fn gelf_messages(datagrams: &[Vec<u8>]) -> Vec<serde_json::Value> {
    use std::io::Read as _;
    let open = |data: &[u8]| -> serde_json::Value {
        let mut text = Vec::new();
        match data {
            [0x1f, 0x8b, ..] => {
                flate2::read::GzDecoder::new(data).read_to_end(&mut text).unwrap();
            }
            [0x78, ..] => {
                flate2::read::ZlibDecoder::new(data)
                    .read_to_end(&mut text)
                    .unwrap();
            }
            _ => text = data.to_vec(),
        }
        serde_json::from_slice(&text).unwrap()
    };
    // Each message's chunks, by its ID: each chunk's number, how many, and its bytes.
    type Chunks = Vec<(u8, u8, Vec<u8>)>;
    let mut chunked: std::collections::BTreeMap<Vec<u8>, Chunks> = Default::default();
    let mut got = Vec::new();
    for d in datagrams {
        match d.as_slice() {
            [0x1e, 0x0f, rest @ ..] => {
                let parts = chunked.entry(rest[..8].to_vec()).or_default();
                parts.push((rest[8], rest[9], rest[10..].to_vec()));
                if parts.len() == usize::from(rest[9]) {
                    parts.sort();
                    let whole: Vec<u8> = parts.iter().flat_map(|p| p.2.clone()).collect();
                    got.push(open(&whole));
                }
            }
            _ => got.push(open(d)),
        }
    }
    got
}

/// A run's log sent to a GELF server (D164), from shards' store by the daemon as syslog's
/// is (D163): over UDP gzipped, as Docker's is unless told, and uncompressed in chunks;
/// over TCP, each message followed by a NUL. Each message is GELF 1.1 with the container's
/// own fields and the attributes asked for, as Docker's gelf driver sends it, stamped with
/// when its output came, to the millisecond.
#[test]
fn a_runs_log_is_sent_to_gelf_as_its_options_say() {
    let Some((home, image)) = home("containers-gelf") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let gelf = |name: &str, opts: &[&str], command: &[&str]| {
        let mut flags = vec!["--name", name, "--log-driver", "gelf", "--label", "team=logs"];
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        let r = run_in(&home, &image, &flags, command);
        assert_eq!(r.status, Some(0), "{name}: {r}");
        r
    };
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_read_timeout(Some(TIMEOUT)).unwrap();
    let address = format!("gelf-address=udp://{}", udp.local_addr().unwrap());
    let receive = |n: usize| {
        let mut datagrams = Vec::new();
        while gelf_messages(&datagrams).len() < n {
            let mut b = vec![0u8; 1 << 16];
            let got = udp.recv(&mut b).unwrap();
            b.truncate(got);
            datagrams.push(b);
        }
        let mut messages = gelf_messages(&datagrams);
        messages.sort_by_key(|m| m["level"].as_u64());
        messages
    };
    gelf(
        "udp",
        &[&address, "labels=team", "tag={{.Name}}"],
        &["streams", "out", "err"],
    );
    let got = receive(2);
    let field = |path: &str, name: &str| {
        let r = shards(&["inspect", "-f", path, name]);
        r.stdout.trim_end().to_string()
    };
    let stamped = shards(&["logs", "-t", "udp"]);
    for (m, (line, level)) in got.iter().zip([("err", 3), ("out", 6)]) {
        assert_eq!(m["version"], "1.1", "{m}");
        assert_eq!(
            (m["short_message"].as_str(), m["level"].as_u64()),
            (Some(line), Some(level)),
            "{m}"
        );
        assert!(!m["host"].as_str().unwrap().is_empty(), "{m}");
        assert_eq!(m["_container_id"], field("{{.Id}}", "udp").as_str(), "{m}");
        assert_eq!(m["_container_name"], "udp", "{m}");
        assert_eq!(m["_image_id"], field("{{.Image}}", "udp").as_str(), "{m}");
        assert_eq!(m["_image_name"], image.as_str(), "{m}");
        assert_eq!(m["_command"], "/bin/testguest streams out err", "{m}");
        assert_eq!(m["_tag"], "udp", "{m}");
        assert_eq!(m["_created"], field("{{.Created}}", "udp").as_str(), "{m}");
        assert_eq!(m["_team"], "logs", "{m}");
        // Its time its output's, as `logs -t` says it, to the millisecond.
        let kept = [&stamped.stdout, &stamped.stderr]
            .iter()
            .flat_map(|s| s.lines())
            .find_map(|l| l.strip_suffix(&format!(" {line}")))
            .unwrap_or_else(|| panic!("{stamped}"));
        let rfc3339 = &time::format_description::well_known::Rfc3339;
        let kept_ms = time::OffsetDateTime::parse(kept, rfc3339)
            .unwrap()
            .unix_timestamp_nanos()
            / 1_000_000;
        let sent_ms = (m["timestamp"].as_f64().unwrap() * 1000.0).round() as i128;
        assert_eq!(sent_ms, kept_ms, "{m}: {stamped}");
    }

    // Uncompressed: a line of 40,000 bytes, in parts of 16 KiB, each its own message in
    // chunks.
    gelf(
        "wide",
        &[&address, "gelf-compression-type=none"],
        &["wide", "40000"],
    );
    let parts: Vec<usize> = receive(3)
        .iter()
        .map(|m| m["short_message"].as_str().unwrap().len())
        .collect();
    assert_eq!(parts, [16384, 16384, 7232]);

    // TCP: each message followed by a NUL.
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("gelf-address=tcp://{}", tcp.local_addr().unwrap());
    let reading = std::thread::spawn(move || {
        let conn = accept_within(&tcp).unwrap();
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut read = BufReader::new(conn);
        let mut got = Vec::new();
        while got.len() < 2 {
            let mut m = Vec::new();
            if read.read_until(0, &mut m).unwrap() == 0 {
                break;
            }
            assert_eq!(m.pop(), Some(0));
            got.push(serde_json::from_slice::<serde_json::Value>(&m).unwrap());
        }
        got
    });
    gelf("tcp", &[&address], &["streams", "out", "err"]);
    let mut got = reading.join().unwrap();
    got.sort_by_key(|m| m["level"].as_u64());
    let lines: Vec<&str> = got.iter().map(|m| m["short_message"].as_str().unwrap()).collect();
    assert_eq!(lines, ["err", "out"]);

    for (opts, said) in [
        (&[][..], "shards: --log-opt gelf-address: where to send the log"),
        (
            &["gelf-address=udp://h"],
            "shards: --log-opt gelf-address=udp://h: a host and its port",
        ),
        (
            &["gelf-address=tcp://h:1", "gelf-compression-type=gzip"],
            "shards: --log-opt gelf-compression-type: compression is UDP's",
        ),
        (
            &["gelf-address=udp://h:1", "gelf-tcp-max-reconnect=2"],
            "shards: --log-opt gelf-tcp-max-reconnect: TCP's",
        ),
    ] {
        let mut flags = vec!["--log-driver", "gelf"];
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        let r = run_in(&home, &image, &flags, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{opts:?}: {r}");
        assert!(r.stderr.starts_with(said), "{opts:?}: {r}");
    }
}

/// The events of Fluentd's forward protocol a server reads on `conn` (Message mode: tag,
/// time, record, options), until `count` have come: each its tag, its time in nanoseconds
/// (whole seconds, or an EventTime's), its record, and its options. Where the options ask
/// for an ack, it is sent back, as fluentd does.
fn fluentd_events(
    conn: &mut (impl std::io::Read + std::io::Write),
    count: usize,
) -> Vec<(String, i128, serde_json::Value, serde_json::Value)> {
    fn read<const N: usize>(r: &mut impl std::io::Read) -> [u8; N] {
        let mut b = [0u8; N];
        r.read_exact(&mut b).unwrap();
        b
    }
    fn value(r: &mut impl std::io::Read) -> serde_json::Value {
        use serde_json::Value;
        let [t] = read::<1>(r);
        let len = |r: &mut dyn FnMut(usize) -> Vec<u8>, n| {
            r(n).iter().fold(0usize, |a, &x| a << 8 | usize::from(x))
        };
        let mut take = |n: usize| {
            let mut b = vec![0u8; n];
            r.read_exact(&mut b).unwrap();
            b
        };
        match t {
            0x00..=0x7f => Value::from(t),
            0x80..=0x8f | 0xde => {
                let n = if t == 0xde {
                    len(&mut take, 2)
                } else {
                    usize::from(t & 0x0f)
                };
                let mut m = serde_json::Map::new();
                for _ in 0..n {
                    let k = value(r);
                    let v = value(r);
                    m.insert(k.as_str().unwrap().to_string(), v);
                }
                Value::Object(m)
            }
            0x90..=0x9f => Value::Array((0..usize::from(t & 0x0f)).map(|_| value(r)).collect()),
            0xa0..=0xbf | 0xd9 | 0xda | 0xdb => {
                let n = match t {
                    0xd9 => len(&mut take, 1),
                    0xda => len(&mut take, 2),
                    0xdb => len(&mut take, 4),
                    _ => usize::from(t & 0x1f),
                };
                Value::String(String::from_utf8(take(n)).unwrap())
            }
            0xd2 => Value::from(i32::from_be_bytes(take(4).try_into().unwrap())),
            0xd7 => {
                assert_eq!(take(1), [0], "EventTime is extension 0");
                let b = take(8);
                let secs = u32::from_be_bytes(b[..4].try_into().unwrap());
                let nanos = u32::from_be_bytes(b[4..].try_into().unwrap());
                serde_json::json!({ "secs": secs, "nanos": nanos })
            }
            t => panic!("a MessagePack form {t:#x} no event holds"),
        }
    }
    let mut events = Vec::new();
    while events.len() < count {
        let e = value(conn);
        let [tag, time, record, option] =
            <[serde_json::Value; 4]>::try_from(e.as_array().unwrap().clone()).unwrap();
        let nanos = match &time {
            serde_json::Value::Number(n) => i128::from(n.as_i64().unwrap()) * 1_000_000_000,
            t => {
                i128::from(t["secs"].as_u64().unwrap()) * 1_000_000_000
                    + i128::from(t["nanos"].as_u64().unwrap())
            }
        };
        if let Some(chunk) = option.get("chunk").and_then(|c| c.as_str()) {
            let mut ack = vec![0x81, 0xa3, b'a', b'c', b'k', 0xa0 | chunk.len() as u8];
            ack.extend_from_slice(chunk.as_bytes());
            conn.write_all(&ack).unwrap();
        }
        events.push((tag.as_str().unwrap().to_string(), nanos, record, option));
    }
    events.sort_by_key(|e| e.2["source"] != "stderr");
    events
}

/// A run's log sent to fluentd (D165), from shards' store by the daemon as syslog's is
/// (D163): each line an event of Fluentd's forward protocol as Docker's fluentd driver
/// sends it: its tag, its time (whole seconds, or to the nanosecond), its record (the
/// container's ID and name, the stream, the line, the attributes asked for, and a part's
/// place in a line longer than 16 KiB), and an ack asked for where told. Over TCP and a
/// unix socket.
#[test]
fn a_runs_log_is_sent_to_fluentd_as_its_options_say() {
    let Some((home, image)) = home("containers-fluentd") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let fluentd = |name: &str, opts: &[&str], command: &[&str]| {
        let mut flags = vec!["--name", name, "--log-driver", "fluentd", "--label", "team=logs"];
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        let r = run_in(&home, &image, &flags, command);
        assert_eq!(r.status, Some(0), "{name}: {r}");
    };
    let rfc3339 = &time::format_description::well_known::Rfc3339;
    let stamped = |name: &str, line: &str| {
        let s = shards(&["logs", "-t", name]);
        let at = [&s.stdout, &s.stderr]
            .iter()
            .flat_map(|o| o.lines())
            .find_map(|l| l.strip_suffix(&format!(" {line}")).map(str::to_string))
            .unwrap_or_else(|| panic!("{s}"));
        time::OffsetDateTime::parse(&at, rfc3339)
            .unwrap()
            .unix_timestamp_nanos()
    };
    // TCP, to the nanosecond, an ack asked for each.
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("fluentd-address={}", tcp.local_addr().unwrap());
    let reading = std::thread::spawn(move || {
        let mut conn = accept_within(&tcp).unwrap();
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        fluentd_events(&mut conn, 2)
    });
    fluentd(
        "nanos",
        &[
            &address,
            "fluentd-sub-second-precision=true",
            "fluentd-request-ack=true",
            "labels=team",
            "tag={{.Name}}",
        ],
        &["streams", "out", "err"],
    );
    let id = shards(&["inspect", "-f", "{{.Id}}", "nanos"])
        .stdout
        .trim()
        .to_string();
    let events = reading.join().unwrap();
    for ((tag, at, record, option), (line, source)) in
        events.iter().zip([("err", "stderr"), ("out", "stdout")])
    {
        // Its tag the one asked for, which its record does not repeat (moby's fluentd sends
        // logger.Info.ExtraAttributes, which leave it out).
        assert_eq!(tag, "nanos", "its tag the one asked for");
        assert_eq!(
            record,
            &serde_json::json!({
                "container_id": id, "container_name": "/nanos", "source": source, "log": line, "team": "logs",
            })
        );
        assert!(
            option
                .get("chunk")
                .is_some_and(|c| c.as_str().is_some_and(|c| c.len() == 24)),
            "{option}"
        );
        assert_eq!(*at, stamped("nanos", line), "its time its output's");
    }

    // A unix socket, whole seconds, a line of 40,000 bytes in parts.
    let sockets = TempDir::new("fd");
    let path = sockets.join("f");
    #[cfg(unix)]
    let unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
    // Windows' AF_UNIX streams, which std does not open: afunix's, Winsock started.
    #[cfg(windows)]
    let unix = {
        shards_vmm::platform::winsock().unwrap();
        shards_vmm::platform::UnixListener::bind(&path).unwrap()
    };
    let reading = std::thread::spawn(move || {
        let (mut conn, _) = unix.accept().unwrap();
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        fluentd_events(&mut conn, 3)
    });
    fluentd(
        "parts",
        &[&format!("fluentd-address=unix:///{}", path.display())],
        &["wide", "40000"],
    );
    let events = reading.join().unwrap();
    let id = events[0].2["partial_id"].clone();
    for (i, (_, at, record, _)) in events.iter().enumerate() {
        assert_eq!(record["log"].as_str().unwrap().len(), [16384, 16384, 7232][i]);
        assert_eq!(record["partial_message"], "true");
        assert_eq!(record["partial_id"], id);
        assert_eq!(record["partial_ordinal"], (i + 1).to_string());
        assert_eq!(record["partial_last"], if i == 2 { "true" } else { "false" });
        assert_eq!(*at % 1_000_000_000, 0, "whole seconds");
    }

    for (opts, said) in [
        (
            &["fluentd-address=http://h:1"][..],
            "shards: --log-opt fluentd-address=http://h:1: the scheme http",
        ),
        (
            &["fluentd-address=tcp://h:1/x"],
            "shards: --log-opt fluentd-address=tcp://h:1/x: a host and a port, without a path",
        ),
        (
            &["fluentd-retry-wait=soon"],
            "shards: --log-opt fluentd-retry-wait=soon: a duration",
        ),
    ] {
        let mut flags = vec!["--log-driver", "fluentd"];
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        let r = run_in(&home, &image, &flags, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{opts:?}: {r}");
        assert!(r.stderr.starts_with(said), "{opts:?}: {r}");
    }
}

/// An entry the journal's socket was sent: its fields in order, and, where it came in a
/// memfd, whether that was sealed against every change, as journald takes one from a
/// sender it cannot vouch for.
#[cfg(target_os = "linux")]
fn journal_entry(journal: &std::os::unix::net::UnixDatagram) -> (Vec<(String, String)>, Option<bool>) {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::FileExt as _;
    let mut data = vec![0u8; 1 << 20];
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    // SAFETY: an all-zero msghdr is valid; its buffers outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: recvmsg(2) into the buffers above, on a socket this test holds.
    let n = unsafe { libc::recvmsg(journal.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    assert!(n >= 0, "{}", std::io::Error::last_os_error());
    data.truncate(usize::try_from(n).unwrap());
    let mut sealed = None;
    // SAFETY: the control message recvmsg wrote, read as CMSG_* read one; its descriptor
    // is new to this process, and owned here.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if !cmsg.is_null() && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let fd = libc::CMSG_DATA(cmsg).cast::<libc::c_int>().read_unaligned();
            let file = std::fs::File::from_raw_fd(fd);
            data = vec![0u8; usize::try_from(file.metadata().unwrap().len()).unwrap()];
            file.read_exact_at(&mut data, 0).unwrap();
            let all = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
            sealed = Some(libc::fcntl(fd, libc::F_GET_SEALS) & all == all);
        }
    }
    let mut fields = Vec::new();
    let mut rest = data.as_slice();
    while !rest.is_empty() {
        let end = rest.iter().position(|&c| c == b'\n' || c == b'=').unwrap();
        let name = String::from_utf8(rest[..end].to_vec()).unwrap();
        let (value, next) = if rest[end] == b'=' {
            let nl = end + 1 + rest[end + 1..].iter().position(|&c| c == b'\n').unwrap();
            (&rest[end + 1..nl], &rest[nl + 1..])
        } else {
            let len =
                usize::try_from(u64::from_le_bytes(rest[end + 1..end + 9].try_into().unwrap())).unwrap();
            let at = end + 9;
            assert_eq!(rest[at + len], b'\n');
            (&rest[at..at + len], &rest[at + len + 1..])
        };
        fields.push((name, String::from_utf8(value.to_vec()).unwrap()));
        rest = next;
    }
    (fields, sealed)
}

/// A run's log sent to the journal (D166), at the socket `SHARDS_JOURNAL_SOCKET` names,
/// where systemd's is not a test's to read: each line an entry of systemd's native
/// protocol, its priority, its line, the container's fields, its output's time and its
/// ordinal; the attributes asked for named as journal fields, and those journald drops or
/// named as the driver's own left out; a long line's parts; an entry too long for a
/// datagram in a sealed memfd. The journal is Linux's: macOS refuses the driver.
#[test]
fn a_runs_log_is_sent_to_journald_as_its_options_say() {
    #[cfg(not(target_os = "linux"))]
    {
        let Some((home, image)) = home("containers-journald") else {
            return;
        };
        let r = run_in(&home, &image, &["--log-driver", "journald"], &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{r}");
        assert!(
            r.stderr
                .starts_with("shards: --log-driver journald: the journal is systemd's, on Linux"),
            "{r}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        if cannot_run_vms() {
            return;
        }
        let sockets = TempDir::new("jd");
        let path = sockets.join("j");
        let journal = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        journal.set_read_timeout(Some(TIMEOUT)).unwrap();
        let (image, _) = served();
        let home = TempDir::new("containers-journald");
        let (kernel, init) = (kernel(), guest_init());
        // The daemon, which the first command starts, told where the journal is.
        let env: [(&str, &OsStr); 4] = [
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel.as_os_str()),
            ("SHARDS_INIT", init.as_os_str()),
            ("SHARDS_JOURNAL_SOCKET", path.as_os_str()),
        ];
        let pulled = run_shards_env(&[], &["pull", "-q", &image], &env, TIMEOUT);
        assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
        let journald = |name: &str, flags: &[&str], command: &[&str]| {
            let mut all = vec!["--name", name, "--log-driver", "journald"];
            all.extend(flags);
            let r = run_in(&home, &image, &all, command);
            assert_eq!(r.status, Some(0), "{name}: {r}");
            shards_in(&home, &["inspect", "-f", "{{.Id}}", name])
                .stdout
                .trim()
                .to_string()
        };
        let field = |fields: &[(String, String)], name: &str| -> Vec<String> {
            fields
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .collect()
        };

        // A line on each stream: its priority, info or err; the container's fields, its tag
        // the short ID; its time its output's; its ordinal from 1. An attribute asked for,
        // named as a field; those left out: one journald drops (a digit first), one named
        // as the driver's own MESSAGE.
        let id = journald(
            "streams",
            &[
                "--label",
                "app.tier=web",
                "--label",
                "1x=y",
                "--label",
                "message=spoof",
                "--log-opt",
                "labels=app.tier,1x,message",
            ],
            &["streams", "out", "err"],
        );
        let short = id.get(..12).unwrap();
        let mut entries = [journal_entry(&journal), journal_entry(&journal)];
        entries.sort_by_key(|(fields, _)| field(fields, "MESSAGE"));
        let stamped = shards_in(&home, &["logs", "-t", "streams"]);
        let rfc3339 = &time::format_description::well_known::Rfc3339;
        let mut ordinals = Vec::new();
        for ((fields, sealed), (line, priority)) in entries.iter().zip([("err", "3"), ("out", "6")]) {
            assert_eq!(*sealed, None, "a datagram: {fields:?}");
            assert_eq!(field(fields, "MESSAGE"), [line], "{fields:?}");
            assert_eq!(field(fields, "PRIORITY"), [priority]);
            assert_eq!(field(fields, "CONTAINER_ID"), [short]);
            assert_eq!(field(fields, "CONTAINER_ID_FULL"), [id.as_str()]);
            assert_eq!(field(fields, "CONTAINER_NAME"), ["streams"]);
            assert_eq!(field(fields, "CONTAINER_TAG"), [short]);
            assert_eq!(field(fields, "SYSLOG_IDENTIFIER"), [short]);
            assert_eq!(field(fields, "IMAGE_NAME"), [image.as_str()]);
            assert_eq!(field(fields, "APP_TIER"), ["web"]);
            assert!(field(fields, "1X").is_empty(), "{fields:?}");
            let epoch = field(fields, "CONTAINER_LOG_EPOCH");
            assert!(epoch.len() == 1 && epoch[0].len() == 64, "{epoch:?}");
            // Its time is its output's, as `logs -t` says it.
            let sent = field(fields, "SYSLOG_TIMESTAMP");
            let sent = time::OffsetDateTime::parse(&sent[0], rfc3339).unwrap();
            let kept = [&stamped.stdout, &stamped.stderr]
                .iter()
                .flat_map(|s| s.lines())
                .find_map(|l| l.strip_suffix(&format!(" {line}")))
                .unwrap_or_else(|| panic!("{stamped}"));
            assert_eq!(
                sent,
                time::OffsetDateTime::parse(kept, rfc3339).unwrap(),
                "{stamped}"
            );
            ordinals.extend(field(fields, "CONTAINER_LOG_ORDINAL"));
        }
        ordinals.sort();
        assert_eq!(ordinals, ["1", "2"]);

        // A line of 40,000 bytes in parts of 16 KiB, as moby's copier cuts it.
        journald("parts", &[], &["wide", "40000"]);
        let parts: Vec<_> = (0..3).map(|_| journal_entry(&journal).0).collect();
        let partial = field(&parts[0], "CONTAINER_PARTIAL_ID");
        assert_eq!(partial[0].len(), 64);
        for (i, fields) in parts.iter().enumerate() {
            assert_eq!(field(fields, "MESSAGE")[0].len(), [16384, 16384, 7232][i]);
            assert_eq!(field(fields, "CONTAINER_PARTIAL_ID"), partial);
            assert_eq!(field(fields, "CONTAINER_PARTIAL_ORDINAL"), [(i + 1).to_string()]);
            let last = i == 2;
            assert_eq!(field(fields, "CONTAINER_PARTIAL_LAST"), [last.to_string()]);
            assert_eq!(field(fields, "CONTAINER_PARTIAL_MESSAGE").is_empty(), last);
        }

        // An entry past the most a datagram takes (twice wmem_max, as Linux doubles a send
        // buffer): in a memfd, sealed.
        let wmem_max: usize = std::fs::read_to_string("/proc/sys/net/core/wmem_max")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let labels = (wmem_max * 2 + 65_536) / 60_000 + 1;
        // A run's request carries at most a message's payload: a host whose datagrams carry
        // more (its wmem_max raised, as Fedora's 4 MiB) cannot be given such an entry.
        if labels * 60_000 > shards_ipc::MAX_PAYLOAD * 3 / 4 {
            eprintln!(
                "SKIP: an entry in a memfd: this host's datagrams carry {} bytes, past what a run's request may",
                wmem_max * 2
            );
            return;
        }
        let file = sockets.join("labels");
        let lines: String = (1..=labels)
            .map(|n| format!("big{n}={}\n", "z".repeat(60_000)))
            .collect();
        std::fs::write(&file, lines).unwrap();
        journald(
            "large",
            &[
                "--label-file",
                file.to_str().unwrap(),
                "--log-opt",
                "labels-regex=^big",
            ],
            &["wide", "10"],
        );
        let (fields, sealed) = journal_entry(&journal);
        assert_eq!(sealed, Some(true), "in a memfd, sealed");
        assert_eq!(field(&fields, "MESSAGE"), ["x".repeat(10)]);
        for n in 1..=labels {
            assert_eq!(field(&fields, &format!("BIG{n}")), ["z".repeat(60_000)]);
        }
    }
}

/// A request a Splunk HTTP Event Collector of the test's was sent: its method, its target,
/// its fields (names in lower case) and its body.
struct HecRequest {
    method: String,
    target: String,
    fields: std::collections::BTreeMap<String, String>,
    body: Vec<u8>,
}

/// A Splunk HTTP Event Collector on `listener`, as HEC answers with indexer
/// acknowledgment: each post its ack's ID, numbered from 0, and each ack asked for
/// indexed. Each connection on a thread of its own, as a driver keeps its own; each
/// request sent on `seen`.
fn hec(listener: std::net::TcpListener, seen: std::sync::mpsc::Sender<HecRequest>) {
    use std::io::{BufRead as _, Read as _, Write as _};
    let posted = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else {
                return;
            };
            let (seen, posted) = (seen.clone(), posted.clone());
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(conn);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let (method, target) = (
                        parts.next().unwrap().to_string(),
                        parts.next().unwrap().to_string(),
                    );
                    let mut fields = std::collections::BTreeMap::new();
                    loop {
                        let mut field = String::new();
                        reader.read_line(&mut field).unwrap();
                        let field = field.trim_end();
                        if field.is_empty() {
                            break;
                        }
                        let (k, v) = field.split_once(':').unwrap();
                        fields.insert(k.to_ascii_lowercase(), v.trim().to_string());
                    }
                    let len: usize = fields.get("content-length").map_or(0, |l| l.parse().unwrap());
                    let mut body = vec![0u8; len];
                    reader.read_exact(&mut body).unwrap();
                    let answer = if target.starts_with("/services/collector/ack") {
                        let asked: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let acks: Vec<String> = asked["acks"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|id| format!("\"{id}\":true"))
                            .collect();
                        format!("{{\"acks\":{{{}}}}}", acks.join(","))
                    } else {
                        let id = posted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        format!("{{\"text\":\"Success\",\"code\":0,\"ackId\":{id}}}")
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{answer}",
                        answer.len()
                    );
                    if reader.get_mut().write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                    let _ = seen.send(HecRequest {
                        method,
                        target,
                        fields,
                        body,
                    });
                }
            });
        }
    });
}

/// The events a post to HEC carried, one JSON object after another.
fn hec_events(body: &[u8]) -> Vec<serde_json::Value> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<serde_json::Value>()
        .map(Result::unwrap)
        .collect()
}

/// A run's log sent to Splunk (D167), to an HTTP Event Collector in the test: each line an
/// event as Docker's driver posts it, its line, source, tag and attributes, its host this
/// one's and its time its output's, with its token; a line posted while its run still
/// runs, as the shipper catches up, where Docker's waits for a tick; with indexer
/// acknowledgment, its ack asked for on the channel it was posted on; the refusals in
/// shards' words.
#[test]
fn a_runs_log_is_sent_to_splunk_as_its_options_say() {
    let Some((home, image)) = home("containers-splunk") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "splunk-url=http://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );
    let (seen, requests) = std::sync::mpsc::channel();
    hec(listener, seen);
    let shards = |args: &[&str]| shards_in(&home, args);
    let splunk = |name: &str, opts: &[&str], command: &[&str], detach: bool| {
        let mut flags = vec![
            "--name",
            name,
            "--log-driver",
            "splunk",
            "--log-opt",
            &url,
            "--log-opt",
            "splunk-token=t",
        ];
        if detach {
            flags.insert(0, "-d");
        }
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        flags.extend(["--label", "team=logs"]);
        let r = run_in(&home, &image, &flags, command);
        assert_eq!(r.status, Some(0), "{name}: {r}");
    };
    // The posts' events, as many as `want`: none waited for past its arrival.
    let posted = |want: usize| -> (Vec<HecRequest>, Vec<serde_json::Value>) {
        let (mut posts, mut events) = (Vec::new(), Vec::new());
        while events.len() < want {
            let request: HecRequest = requests.recv_timeout(TIMEOUT).unwrap();
            if request.target == "/services/collector/event/1.0" {
                events.extend(hec_events(&request.body));
            }
            posts.push(request);
        }
        (posts, events)
    };

    // A line on each stream: its fields, the label asked for, its token.
    splunk("inline", &["labels=team"], &["streams", "out", "err"], false);
    let (posts, mut events) = posted(2);
    let id = shards(&["inspect", "-f", "{{.Id}}", "inline"])
        .stdout
        .trim()
        .to_string();
    let stamped = shards(&["logs", "-t", "inline"]);
    events.sort_by_key(|e| e["event"]["line"].as_str().unwrap().to_string());
    for (event, (line, source)) in events.iter().zip([("err", "stderr"), ("out", "stdout")]) {
        assert_eq!(
            event["event"],
            serde_json::json!({"line": line, "source": source, "tag": id.get(..12).unwrap(), "attrs": {"team": "logs"}}),
            "{event}"
        );
        let host = event["host"].as_str().unwrap();
        assert!(!host.is_empty() && !host.contains(':'), "a host's name: {event}");
        // Its time its output's, as `logs -t` says it, to the microsecond.
        let kept = [&stamped.stdout, &stamped.stderr]
            .iter()
            .flat_map(|s| s.lines())
            .find_map(|l| l.strip_suffix(&format!(" {line}")))
            .unwrap_or_else(|| panic!("{stamped}"));
        let kept = time::OffsetDateTime::parse(kept, &time::format_description::well_known::Rfc3339)
            .unwrap()
            .unix_timestamp_nanos();
        let (secs, micros) = event["time"].as_str().unwrap().split_once('.').unwrap();
        let sent =
            i128::from(secs.parse::<i64>().unwrap()) * 1_000_000 + i128::from(micros.parse::<i64>().unwrap());
        assert!((sent - kept / 1000).abs() <= 1, "{event}: {stamped}");
    }
    for post in &posts {
        assert_eq!(post.method, "POST");
        assert_eq!(post.fields["authorization"], "Splunk t");
    }

    // A line posted while its run still runs: the shipper caught up with its log.
    splunk("live", &[], &["ignore", "TERM"], true);
    let (_, events) = posted(1);
    assert_eq!(events[0]["event"]["line"], "ready");
    assert_eq!(
        shards(&["inspect", "-f", "{{.State.Running}}", "live"])
            .stdout
            .trim(),
        "true"
    );
    assert_eq!(shards(&["rm", "-f", "live"]).status, Some(0));

    // Indexer acknowledgment: posted on a channel, its ack asked for there.
    splunk(
        "acked",
        &["splunk-index-acknowledgment=true"],
        &["streams", "out", "err"],
        false,
    );
    let mut asked = Vec::new();
    let mut channels = std::collections::BTreeSet::new();
    let mut lines = 0;
    while lines < 2 || asked.is_empty() {
        let request: HecRequest = requests.recv_timeout(TIMEOUT).unwrap();
        channels.insert(request.fields["x-splunk-request-channel"].clone());
        if request.target.starts_with("/services/collector/ack") {
            asked.push(request.target.clone());
        } else {
            lines += hec_events(&request.body).len();
        }
    }
    let channel = channels.iter().next().unwrap().clone();
    assert_eq!(channels.len(), 1, "one channel: {channels:?}");
    assert_eq!(asked[0], format!("/services/collector/ack?channel={channel}"));

    // A file not found exits 127 where its OS's words say so: Windows' "The system cannot
    // find the file specified" among docker/cli's (run.go `toStatusError`), Unix's "No such
    // file or directory" not, its first letter upper case.
    let not_found = if cfg!(windows) { 127 } else { 125 };
    for (opts, said, status) in [
        (
            &["splunk-insecureskipverify=true"][..],
            "shards: --log-opt splunk-insecureskipverify: shards posts a log only to a Splunk whose certificate it verifies",
            125,
        ),
        (
            &["splunk-capath=/nonexistent"],
            "shards: --log-opt splunk-capath=/nonexistent: ",
            not_found,
        ),
        (
            &["splunk-format=xml"],
            "shards: --log-opt splunk-format=xml: inline, json or raw",
            125,
        ),
    ] {
        let mut flags = vec![
            "--log-driver",
            "splunk",
            "--log-opt",
            &url,
            "--log-opt",
            "splunk-token=t",
        ];
        for o in opts {
            flags.extend(["--log-opt", o]);
        }
        let r = run_in(&home, &image, &flags, &["exit", "0"]);
        assert_eq!(r.status, Some(status), "{opts:?}: {r}");
        assert!(r.stderr.starts_with(said), "{opts:?}: {r}");
    }
}

/// A request CloudWatch Logs was sent in a test.
struct CloudWatchRequest {
    target: String,
    fields: std::collections::BTreeMap<String, String>,
    body: serde_json::Value,
}

/// CloudWatch Logs on `listener`, as its JSON 1.1 API answers: the group `existing` there
/// from the start, others once made; a stream made in a group not there refused as
/// ResourceNotFoundException, one made twice as ResourceAlreadyExistsException; events
/// put to a stream there. Each connection on a thread of its own, as a client keeps its
/// own; each request sent on `seen`.
fn cloudwatch(listener: std::net::TcpListener, seen: std::sync::mpsc::Sender<CloudWatchRequest>) {
    use std::io::{BufRead as _, Read as _, Write as _};
    type Made = (
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<(String, String)>,
    );
    let made: std::sync::Arc<std::sync::Mutex<Made>> = std::sync::Arc::new(std::sync::Mutex::new((
        ["existing".to_string()].into(),
        std::collections::BTreeSet::new(),
    )));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else {
                return;
            };
            let (seen, made) = (seen.clone(), made.clone());
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(conn);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut fields = std::collections::BTreeMap::new();
                    loop {
                        let mut field = String::new();
                        reader.read_line(&mut field).unwrap();
                        let field = field.trim_end();
                        if field.is_empty() {
                            break;
                        }
                        let (k, v) = field.split_once(':').unwrap();
                        fields.insert(k.to_ascii_lowercase(), v.trim().to_string());
                    }
                    let len: usize = fields.get("content-length").map_or(0, |l| l.parse().unwrap());
                    let mut body = vec![0u8; len];
                    reader.read_exact(&mut body).unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let target = fields.get("x-amz-target").cloned().unwrap_or_default();
                    let text = |k: &str| body[k].as_str().unwrap_or_default().to_string();
                    let (group, stream) = (text("logGroupName"), text("logStreamName"));
                    let refused = |kind: &str, message: &str| {
                        (
                            400,
                            format!("{{\"__type\":\"{kind}\",\"message\":\"{message}\"}}"),
                        )
                    };
                    let (status, answer) = {
                        let mut made = made.lock().unwrap();
                        match target.as_str() {
                            "Logs_20140328.CreateLogGroup" => {
                                made.0.insert(group);
                                (200, "{}".to_string())
                            }
                            "Logs_20140328.CreateLogStream" if !made.0.contains(&group) => refused(
                                "ResourceNotFoundException",
                                "The specified log group does not exist.",
                            ),
                            "Logs_20140328.CreateLogStream"
                                if !made.1.insert((group.clone(), stream.clone())) =>
                            {
                                refused(
                                    "ResourceAlreadyExistsException",
                                    "The specified log stream already exists",
                                )
                            }
                            "Logs_20140328.CreateLogStream" => (200, "{}".to_string()),
                            "Logs_20140328.PutLogEvents"
                                if !made.1.contains(&(group.clone(), stream.clone())) =>
                            {
                                refused(
                                    "ResourceNotFoundException",
                                    "The specified log stream does not exist.",
                                )
                            }
                            "Logs_20140328.PutLogEvents" => {
                                (200, "{\"nextSequenceToken\":\"t\"}".to_string())
                            }
                            _ => refused("UnknownOperationException", "unknown"),
                        }
                    };
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/x-amz-json-1.1\r\nContent-Length: {}\r\n\r\n{answer}",
                        answer.len()
                    );
                    // Seen before it is answered: what its answer lets the client ask next
                    // comes after it (a reply first let the next connection's be seen
                    // before it on a loaded host).
                    let _ = seen.send(CloudWatchRequest { target, fields, body });
                    if reader.get_mut().write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
}

/// A run's log sent to CloudWatch Logs (D168), to CloudWatch in the test: its stream made,
/// named for its container's whole ID; each line an event at its output's time, to the
/// millisecond; signed with what the environment of the client that started the run
/// finds, not the daemon's: a second client's keys its own, a credential process run in
/// its client's environment; its group made where the options say; lines joined where a
/// pattern says; a line put while its run still runs; nothing of the client's secrets in
/// the request the daemon keeps on disk; the refusals in shards' words.
#[test]
fn a_runs_log_is_sent_to_cloudwatch_logs_as_its_options_say() {
    let Some((home, image)) = home("containers-awslogs") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "awslogs-endpoint=http://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );
    let (seen, requests) = std::sync::mpsc::channel();
    cloudwatch(listener, seen);
    let (kernel, init) = (kernel(), guest_init());
    let config = home.join("aws-config");
    std::fs::write(
        &config,
        "[profile process]\ncredential_process = printf '{\"Version\": 1, \"AccessKeyId\": \"PROCESS\", \"SecretAccessKey\": \"process-secret\"}'\n",
    )
    .unwrap();
    let none = home.join("no-aws-credentials");
    let lines = home.join("lines");
    std::fs::create_dir_all(&lines).unwrap();
    std::fs::write(
        lines.join("lines.txt"),
        "1 first\n  more\n2 second\n  more\n3 third\n",
    )
    .unwrap();
    let first: &[(&str, &str)] = &[
        ("AWS_ACCESS_KEY_ID", "AKIDFIRST"),
        ("AWS_SECRET_ACCESS_KEY", "first-secret-key"),
        ("AWS_SESSION_TOKEN", "first-session-token"),
        (
            "DOCKER_AUTH_CONFIG",
            r#"{"auths":{"registry.test":{"auth":"cmVnaXN0cnktc2VjcmV0"}}}"#,
        ),
    ];
    // A client's environment: the run's home, and of AWS's only what `aws` gives; whatever
    // the test's own environment holds of AWS's is set empty, which is none.
    let shards_with = |aws: &[(&str, &str)], args: &[&str]| {
        let mut env: Vec<(&str, std::ffi::OsString)> = vec![
            ("SHARDS_HOME", home.as_os_str().to_owned()),
            ("SHARDS_KERNEL", kernel.as_os_str().to_owned()),
            ("SHARDS_INIT", init.as_os_str().to_owned()),
            ("AWS_CONFIG_FILE", config.as_os_str().to_owned()),
            ("AWS_SHARED_CREDENTIALS_FILE", none.as_os_str().to_owned()),
        ];
        for k in [
            "AWS_PROFILE",
            "AWS_DEFAULT_PROFILE",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_CA_BUNDLE",
            "AWS_ENDPOINT_URL",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        ] {
            env.push((k, std::ffi::OsString::new()));
        }
        env.push(("AWS_EC2_METADATA_DISABLED", "true".into()));
        for (k, v) in aws {
            env.retain(|(name, _)| name != k);
            env.push((k, (*v).into()));
        }
        let env: Vec<(&str, &std::ffi::OsStr)> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        run_shards_env(&[], args, &env, TIMEOUT)
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let id_of = |name: &str| {
        shards(&["inspect", "-f", "{{.Id}}", name])
            .stdout
            .trim()
            .to_string()
    };
    let awslogs = |name: &str, flags: &[&str], opts: &[&str], command: &[&str], aws: &[(&str, &str)]| {
        let mut args = vec![
            "run",
            "--pull",
            "never",
            "--name",
            name,
            "--log-driver",
            "awslogs",
            "--log-opt",
            "awslogs-region=us-east-1",
            "--log-opt",
            &endpoint,
        ];
        args.extend(flags);
        for o in opts {
            args.extend(["--log-opt", o]);
        }
        args.push(&image);
        args.extend(command);
        shards_with(aws, &args)
    };
    // The requests asked of CloudWatch until `stream` has `want` events put: none waited
    // for past its arrival.
    let put = |stream: &str, want: usize| -> (Vec<CloudWatchRequest>, Vec<serde_json::Value>) {
        let (mut asked, mut events) = (Vec::new(), Vec::new());
        while events.len() < want {
            let r: CloudWatchRequest = requests.recv_timeout(TIMEOUT).unwrap();
            if r.target == "Logs_20140328.PutLogEvents" && r.body["logStreamName"] == stream {
                events.extend(r.body["logEvents"].as_array().unwrap().iter().cloned());
            }
            asked.push(r);
        }
        (asked, events)
    };
    let signed_by = |asked: &[CloudWatchRequest], key: &str| {
        for r in asked {
            let auth = &r.fields["authorization"];
            assert!(
                auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={key}/"))
                    && auth.contains("/us-east-1/logs/aws4_request"),
                "{}: {auth}",
                r.target
            );
            assert_eq!(r.fields["content-type"], "application/x-amz-json-1.1");
        }
    };

    // A line on each stream: its stream made first, for its container's whole ID; each
    // line an event at its output's time; signed with this client's keys and token.
    let ran = awslogs(
        "first",
        &[],
        &["awslogs-group=existing"],
        &["streams", "out", "err"],
        first,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let id = id_of("first");
    let (asked, events) = put(&id, 2);
    assert_eq!(asked[0].target, "Logs_20140328.CreateLogStream");
    assert_eq!(
        asked[0].body,
        serde_json::json!({"logGroupName": "existing", "logStreamName": id})
    );
    signed_by(&asked, "AKIDFIRST");
    assert!(
        asked
            .iter()
            .all(|r| r.fields["x-amz-security-token"] == "first-session-token")
    );
    let stamped = shards(&["logs", "-t", "first"]);
    let mut messages: Vec<&str> = events.iter().map(|e| e["message"].as_str().unwrap()).collect();
    messages.sort_unstable();
    assert_eq!(messages, ["err", "out"]);
    for e in &events {
        let line = e["message"].as_str().unwrap();
        let kept = [&stamped.stdout, &stamped.stderr]
            .iter()
            .flat_map(|s| s.lines())
            .find_map(|l| l.strip_suffix(&format!(" {line}")))
            .unwrap_or_else(|| panic!("{stamped}"));
        let kept = time::OffsetDateTime::parse(kept, &time::format_description::well_known::Rfc3339)
            .unwrap()
            .unix_timestamp_nanos();
        assert_eq!(
            i128::from(e["timestamp"].as_i64().unwrap()),
            kept / 1_000_000,
            "{e}"
        );
    }
    // Nothing of the client's secrets in the request the container keeps.
    let kept = std::fs::read(home.join("containers").join(&id).join("request")).unwrap();
    for secret in ["first-secret-key", "first-session-token", "cmVnaXN0cnktc2VjcmV0"] {
        assert!(
            !kept.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "{secret} kept on disk"
        );
    }

    // Another client's keys: its run's own, the daemon started by another.
    let second = [
        ("AWS_ACCESS_KEY_ID", "AKIDSECOND"),
        ("AWS_SECRET_ACCESS_KEY", "second-secret-key"),
    ];
    let ran = awslogs(
        "second",
        &[],
        &["awslogs-group=existing"],
        &["streams", "a", "b"],
        &second,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let (asked, _) = put(&id_of("second"), 2);
    signed_by(&asked, "AKIDSECOND");
    assert!(
        asked
            .iter()
            .all(|r| !r.fields.contains_key("x-amz-security-token"))
    );

    // A credential process, run by the daemon in its client's environment.
    let ran = awslogs(
        "processed",
        &[],
        &["awslogs-group=existing"],
        &["streams", "c", "d"],
        &[("AWS_PROFILE", "process")],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let (asked, _) = put(&id_of("processed"), 2);
    signed_by(&asked, "PROCESS");

    // A group not there, made as the options say, then its stream.
    let ran = awslogs(
        "made",
        &[],
        &["awslogs-group=fresh", "awslogs-create-group=true"],
        &["streams", "e", "f"],
        first,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let (asked, _) = put(&id_of("made"), 2);
    let targets: Vec<&str> = asked.iter().map(|r| r.target.as_str()).collect();
    assert_eq!(
        targets.get(..3).unwrap(),
        [
            "Logs_20140328.CreateLogStream",
            "Logs_20140328.CreateLogGroup",
            "Logs_20140328.CreateLogStream"
        ],
        "{targets:?}"
    );

    // Lines joined where a pattern says where an event starts: the lines in its own files,
    // put there (`cp`) before it starts, as every host serves.
    let made = shards_with(
        first,
        &[
            "create",
            "--pull",
            "never",
            "--name",
            "joined",
            "--log-driver",
            "awslogs",
            "--log-opt",
            "awslogs-region=us-east-1",
            "--log-opt",
            &endpoint,
            "--log-opt",
            "awslogs-group=existing",
            "--log-opt",
            "awslogs-multiline-pattern=^[0-9]",
            &image,
            "lines",
            "/lines.txt",
            "",
        ],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let source = lines.join("lines.txt").display().to_string();
    let copied = shards(&["cp", &source, "joined:/lines.txt"]);
    assert_eq!(copied.status, Some(0), "{copied}");
    let ran = shards_with(first, &["start", "-a", "joined"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    let (_, events) = put(&id_of("joined"), 3);
    let messages: Vec<&str> = events.iter().map(|e| e["message"].as_str().unwrap()).collect();
    assert_eq!(messages, ["1 first\n  more\n", "2 second\n  more\n", "3 third\n"]);

    // A line put while its run still runs: its shipper caught up with its log.
    let ran = awslogs(
        "live",
        &["-d"],
        &["awslogs-group=existing"],
        &["ignore", "TERM"],
        first,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let (_, events) = put(&id_of("live"), 1);
    assert_eq!(events[0]["message"], "ready");
    assert_eq!(
        shards(&["inspect", "-f", "{{.State.Running}}", "live"])
            .stdout
            .trim(),
        "true"
    );
    assert_eq!(shards(&["rm", "-f", "live"]).status, Some(0));

    for (opts, said) in [
        (
            &[][..],
            "shards: --log-opt awslogs-group: the log group to send the log to",
        ),
        (
            &["awslogs-group=existing", "awslogs-format=xml"],
            "shards: --log-opt awslogs-format=xml: json/emf, or none",
        ),
        (
            &["awslogs-group=a b"],
            "shards: --log-opt awslogs-group=a b: a log group's name",
        ),
    ] {
        let r = awslogs("refused", &[], opts, &["exit", "0"], first);
        assert_eq!(r.status, Some(125), "{opts:?}: {r}");
        assert!(r.stderr.starts_with(said), "{opts:?}: {r}");
    }
}

/// A request Google was sent in a test: its method, target, fields and body.
struct GoogleRequest {
    method: String,
    target: String,
    fields: std::collections::BTreeMap<String, String>,
    body: String,
}

/// Google on `listener`, as a run's log reaches it: an OAuth token endpoint, which gives a
/// refresh token's access token; Cloud Logging's entries:write, which takes any; and
/// GCE's metadata server, its project, its instance and its default account's token. Each
/// connection on a thread of its own; each request sent on `seen`.
fn google(listener: std::net::TcpListener, seen: std::sync::mpsc::Sender<GoogleRequest>) {
    use std::io::{BufRead as _, Read as _, Write as _};
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else {
                return;
            };
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(conn);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let (method, target) = (
                        parts.next().unwrap().to_string(),
                        parts.next().unwrap().to_string(),
                    );
                    let mut fields = std::collections::BTreeMap::new();
                    loop {
                        let mut field = String::new();
                        reader.read_line(&mut field).unwrap();
                        let field = field.trim_end();
                        if field.is_empty() {
                            break;
                        }
                        let (k, v) = field.split_once(':').unwrap();
                        fields.insert(k.to_ascii_lowercase(), v.trim().to_string());
                    }
                    let len: usize = fields.get("content-length").map_or(0, |l| l.parse().unwrap());
                    let mut body = vec![0u8; len];
                    reader.read_exact(&mut body).unwrap();
                    let path = target.split('?').next().unwrap_or_default();
                    let answer = match path {
                        "/token" => {
                            r#"{"access_token":"ya29.refreshed","expires_in":3599,"token_type":"Bearer"}"#
                        }
                        "/v2/entries:write" => "{}",
                        "/computeMetadata/v1/project/project-id" => "meta-project-1",
                        "/computeMetadata/v1/instance/zone" => "projects/123/zones/us-central1-a",
                        "/computeMetadata/v1/instance/name" => "gce-vm",
                        "/computeMetadata/v1/instance/id" => "4242",
                        "/computeMetadata/v1/instance/service-accounts/default/token" => {
                            r#"{"access_token":"ya29.instance","expires_in":3599,"token_type":"Bearer"}"#
                        }
                        _ => "",
                    };
                    let status = if answer.is_empty() {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nMetadata-Flavor: Google\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{answer}",
                        answer.len()
                    );
                    // Seen before it is answered: the token its answer gives is used on
                    // another connection, whose writes were seen before it on a loaded
                    // host (dev CI, run 38651721133).
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let _ = seen.send(GoogleRequest {
                        method,
                        target,
                        fields,
                        body,
                    });
                    if reader.get_mut().write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
}

/// The entries of macOS's unified log under subsystem `shards` and `category`, each its type
/// and its message, as `log show` gives them, waiting up to TIMEOUT for `count`: a run's
/// lines are sent from the store after it, not as it ends.
#[cfg(target_os = "macos")]
fn unified_log(category: &str, count: usize) -> Vec<(String, String)> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let out = common::output_within(
            std::process::Command::new("/usr/bin/log").args([
                "show",
                "--last",
                "10m",
                "--style",
                "ndjson",
                "--predicate",
                &format!("subsystem == \"shards\" AND category == \"{category}\""),
            ]),
            TIMEOUT,
        );
        let entries: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["category"] == category)
            .map(|e| {
                (
                    e["messageType"].as_str().unwrap_or_default().to_string(),
                    e["eventMessage"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        if entries.len() >= count || Instant::now() > deadline {
            return entries;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A run's log sent to macOS's own log (oslog, D199): each line an entry of its unified log,
/// subsystem `shards`, category the run's tag, stdout's of the default type and stderr's of
/// the error type, a line longer than the 1,015 bytes os_log keeps whole in parts of at most
/// that many (PM M374); and syslog's local default there the same, macOS's syslogd keeping
/// nothing sent to its socket. Refused elsewhere in shards' words.
#[test]
fn a_runs_log_is_sent_to_macos_own_log_as_its_options_say() {
    let Some((home, image)) = home("containers-oslog") else {
        return;
    };
    #[cfg(not(target_os = "macos"))]
    {
        let r = run_in(&home, &image, &["--log-driver", "oslog"], &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{r}");
        assert!(
            r.stderr
                .contains("--log-driver oslog: the unified log is macOS's own"),
            "{r}"
        );
    }
    #[cfg(target_os = "macos")]
    {
        let tag = format!(
            "shards-test-{}-{}",
            std::process::id(),
            home.file_name().unwrap().to_string_lossy()
        );
        let ran = |driver: &str, tag: &str, command: &[&str]| {
            let r = run_in(
                &home,
                &image,
                &["--log-driver", driver, "--log-opt", &format!("tag={tag}")],
                command,
            );
            assert_eq!(r.status, Some(0), "{r}");
        };
        ran("oslog", &tag, &["streams", "out", "err"]);
        let mut got = unified_log(&tag, 2);
        got.sort();
        assert_eq!(
            got,
            [("Default".into(), "out".into()), ("Error".into(), "err".into())],
            "{got:?}"
        );
        let wide = format!("{tag}-wide");
        ran("oslog", &wide, &["wide", "3000"]);
        let parts: Vec<usize> = unified_log(&wide, 3).iter().map(|(_, m)| m.len()).collect();
        assert_eq!(parts, [1015, 1015, 970]);
        let local = format!("{tag}-syslog");
        ran("syslog", &local, &["streams", "out", "err"]);
        let mut got = unified_log(&local, 2);
        got.sort();
        assert_eq!(
            got,
            [("Default".into(), "out".into()), ("Error".into(), "err".into())],
            "{got:?}"
        );
    }
}

/// A real-time ETW session of a test's own that listens for moby's etwlogs provider
/// {a3693192-9ed6-46d2-a981-f8226c8363bd}: each event's string as it comes, until the
/// session is dropped, which stops it.
#[cfg(windows)]
struct EtwSession {
    name: Vec<u16>,
    control: windows_sys::Win32::System::Diagnostics::Etw::CONTROLTRACE_HANDLE,
    trace: u64,
    /// EVENT_TRACE_PROPERTIES, then room for the session's name, 8-byte aligned.
    properties: Vec<u64>,
    events: Box<std::sync::Mutex<Vec<String>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl EtwSession {
    /// None where this host's user may start no session, which takes an administrator or
    /// one of Performance Log Users (StartTrace's ERROR_ACCESS_DENIED), said with a SKIP line.
    fn start() -> Option<EtwSession> {
        use std::io::Write as _;
        use windows_sys::Win32::System::Diagnostics::Etw::*;
        const PROVIDER: windows_sys::core::GUID =
            windows_sys::core::GUID::from_u128(0xa3693192_9ed6_46d2_a981_f8226c8363bd);
        unsafe extern "system" fn heard(record: *mut EVENT_RECORD) {
            // SAFETY: the record ETW hands its callback, valid for the call.
            let r = unsafe { &*record };
            let id = r.EventHeader.ProviderId;
            if (id.data1, id.data2, id.data3, id.data4)
                != (PROVIDER.data1, PROVIDER.data2, PROVIDER.data3, PROVIDER.data4)
            {
                return;
            }
            // SAFETY: the session's Context, the events' list, which outlives its reader;
            // and the event's data, its UTF-16 string, of the length it says.
            let (events, words) = unsafe {
                (
                    &*(r.UserContext as *const std::sync::Mutex<Vec<String>>),
                    std::slice::from_raw_parts(r.UserData as *const u16, usize::from(r.UserDataLength) / 2),
                )
            };
            let text = String::from_utf16_lossy(words).trim_end_matches('\0').to_string();
            events.lock().unwrap().push(text);
        }
        let name: Vec<u16> = format!("shards-test-etw-{}", std::process::id())
            .encode_utf16()
            .chain([0])
            .collect();
        let head = size_of::<EVENT_TRACE_PROPERTIES>();
        let bytes = head + name.len() * 2;
        let mut properties = vec![0u64; bytes.div_ceil(8)];
        let p = properties.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        // SAFETY: zeroed properties of the size they say, the name's room after them.
        unsafe {
            (*p).Wnode.BufferSize = u32::try_from(bytes).unwrap();
            (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*p).Wnode.ClientContext = 1;
            (*p).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            (*p).LoggerNameOffset = u32::try_from(head).unwrap();
        }
        let mut control = CONTROLTRACE_HANDLE::default();
        // SAFETY: a NUL-terminated name and the properties above.
        let started = unsafe { StartTraceW(&raw mut control, name.as_ptr(), p) };
        if started == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED {
            let _ = writeln!(
                std::io::stderr(),
                "SKIP: an ETW session is an administrator's or Performance Log Users', and this host's user is neither"
            );
            return None;
        }
        assert_eq!(started, 0, "StartTraceW");
        // SAFETY: the session just started, and the provider's GUID.
        let enabled = unsafe {
            EnableTraceEx2(
                control,
                &PROVIDER,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                u8::try_from(TRACE_LEVEL_VERBOSE).unwrap(),
                0,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert_eq!(enabled, 0, "EnableTraceEx2");
        let events = Box::new(std::sync::Mutex::new(Vec::new()));
        let mut logfile = EVENT_TRACE_LOGFILEW {
            LoggerName: name.as_ptr().cast_mut(),
            Anonymous1: EVENT_TRACE_LOGFILEW_0 {
                ProcessTraceMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
            },
            Anonymous2: EVENT_TRACE_LOGFILEW_1 {
                EventRecordCallback: Some(heard),
            },
            Context: std::ptr::from_ref(&*events).cast_mut().cast(),
            ..Default::default()
        };
        // SAFETY: the logfile above, its name and context outliving the trace.
        let trace = unsafe { OpenTraceW(&raw mut logfile) }.Value;
        assert_ne!(trace, u64::MAX, "OpenTraceW");
        let reader = std::thread::spawn(move || {
            let handle = PROCESSTRACE_HANDLE { Value: trace };
            // SAFETY: the trace opened above, read until the session stops.
            unsafe { ProcessTrace(&handle, 1, std::ptr::null(), std::ptr::null()) };
        });
        Some(EtwSession {
            name,
            control,
            trace,
            properties,
            events,
            reader: Some(reader),
        })
    }

    /// The events' strings come so far, waiting up to TIMEOUT for `count`.
    fn events(&self, count: usize) -> Vec<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let got = self.events.lock().unwrap().clone();
            if got.len() >= count || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(windows)]
impl Drop for EtwSession {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Diagnostics::Etw::*;
        // SAFETY: the session this started, stopped by its name with its properties; its
        // trace closed, which ends the reader's ProcessTrace.
        unsafe {
            ControlTraceW(
                self.control,
                self.name.as_ptr(),
                self.properties.as_mut_ptr().cast(),
                EVENT_TRACE_CONTROL_STOP,
            );
            CloseTrace(PROCESSTRACE_HANDLE { Value: self.trace });
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// A run's log sent to Event Tracing for Windows (etwlogs): each line an event of moby's
/// etwlogs provider, its words moby's (`container_name: ..., log: LINE`), as a session
/// that listens for the provider has it; its own copy kept, `logs` reading it. Refused
/// elsewhere in shards' words: ETW is Windows' own.
#[test]
fn a_runs_log_is_sent_to_etw_as_its_options_say() {
    let Some((home, image)) = home("containers-etw") else {
        return;
    };
    #[cfg(not(windows))]
    {
        let r = run_in(&home, &image, &["--log-driver", "etwlogs"], &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{r}");
        assert!(
            r.stderr
                .contains("--log-driver etwlogs: Event Tracing for Windows is Windows' own"),
            "{r}"
        );
    }
    #[cfg(windows)]
    {
        let Some(session) = EtwSession::start() else {
            return;
        };
        let r = run_in(
            &home,
            &image,
            &["--name", "etw", "--log-driver", "etwlogs"],
            &["streams", "out", "err"],
        );
        assert_eq!(r.status, Some(0), "{r}");
        let id = shards_in(&home, &["inspect", "-f", "{{.Id}}", "etw"]).stdout;
        let image_id = shards_in(&home, &["inspect", "-f", "{{.Image}}", "etw"]).stdout;
        let head = format!(
            "container_name: etw, image_name: {image}, container_id: {}, image_id: {}, ",
            id.trim(),
            image_id.trim()
        );
        let mut events = session.events(2);
        events.sort();
        assert_eq!(
            events,
            [
                format!("{head}source: stderr, log: err"),
                format!("{head}source: stdout, log: out")
            ]
        );
        assert_eq!(shards_in(&home, &["logs", "etw"]).stdout, "out\n");
    }
}

/// A run's log sent to Cloud Logging (D169), to Google in the test: each line an entry at
/// its output's time, its payload the run's details and its line, its stream a label;
/// written with an access token the environment of the client that started the run finds:
/// a user's refresh token in its credentials (their quota project sent with each write),
/// or GCE's metadata server, whose project and instance it is logged for; under shards'
/// own log name, or the one the options give; the refusals in shards' words.
#[test]
fn a_runs_log_is_sent_to_cloud_logging_as_its_options_say() {
    let Some((home, image)) = home("containers-gcplogs") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let endpoint = format!("gcp-endpoint=http://{at}");
    let (seen, requests) = std::sync::mpsc::channel();
    google(listener, seen);
    let (kernel, init) = (kernel(), guest_init());
    let credentials = home.join("adc.json");
    std::fs::write(
        &credentials,
        format!(
            r#"{{"type":"authorized_user","client_id":"client","client_secret":"secret","refresh_token":"the-refresh-token","token_uri":"http://{at}/token","quota_project_id":"quota-project-1"}}"#
        ),
    )
    .unwrap();
    let gcloud = home.join("gcloud");
    std::fs::create_dir_all(&gcloud).unwrap();
    // A client's environment: its home's, and of Google's only what `google` gives;
    // whatever the test's own holds of Google's is set empty, which is none.
    let shards_with = |google: &[(&str, &str)], args: &[&str]| {
        let mut env: Vec<(&str, std::ffi::OsString)> = vec![
            ("SHARDS_HOME", home.as_os_str().to_owned()),
            ("SHARDS_KERNEL", kernel.as_os_str().to_owned()),
            ("SHARDS_INIT", init.as_os_str().to_owned()),
            ("CLOUDSDK_CONFIG", gcloud.as_os_str().to_owned()),
        ];
        for k in [
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_QUOTA_PROJECT",
            "GCE_METADATA_HOST",
        ] {
            env.push((k, std::ffi::OsString::new()));
        }
        for (k, v) in google {
            env.retain(|(name, _)| name != k);
            env.push((k, (*v).into()));
        }
        let env: Vec<(&str, &std::ffi::OsStr)> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        run_shards_env(&[], args, &env, TIMEOUT)
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let gcplogs = |name: &str, opts: &[&str], command: &[&str], google: &[(&str, &str)]| {
        let mut args = vec![
            "run",
            "--pull",
            "never",
            "--name",
            name,
            "--log-driver",
            "gcplogs",
            "--log-opt",
            &endpoint,
        ];
        for o in opts {
            args.extend(["--log-opt", o]);
        }
        args.push(&image);
        args.extend(command);
        shards_with(google, &args)
    };
    // The requests asked of Google until `want` entries are written: none waited for past
    // its arrival.
    let written = |want: usize| -> (Vec<GoogleRequest>, Vec<serde_json::Value>) {
        let (mut asked, mut entries) = (Vec::new(), Vec::new());
        while entries.len() < want {
            let r: GoogleRequest = requests.recv_timeout(TIMEOUT).unwrap();
            if r.target == "/v2/entries:write" {
                let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
                entries.extend(body["entries"].as_array().unwrap().iter().cloned());
            }
            asked.push(r);
        }
        (asked, entries)
    };

    // A user's credentials: their refresh token's access token, their quota project with
    // each write; the project the options name, its `global` resource; each line an entry
    // of the run's details, at its output's time, its stream a label.
    let adc = credentials.display().to_string();
    let ran = gcplogs(
        "user",
        &["gcp-project=my-project-1", "gcp-log-cmd=true", "labels=team"],
        &["streams", "out", "err"],
        &[("GOOGLE_APPLICATION_CREDENTIALS", &adc)],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let (asked, entries) = written(2);
    let token = asked
        .iter()
        .find(|r| r.target == "/token")
        .expect("a token asked for");
    assert!(
        token.body.contains("grant_type=refresh_token")
            && token.body.contains("refresh_token=the-refresh-token"),
        "{}",
        token.body
    );
    let id = shards(&["inspect", "-f", "{{.Id}}", "user"])
        .stdout
        .trim()
        .to_string();
    for w in asked.iter().filter(|r| r.target == "/v2/entries:write") {
        assert_eq!(w.method, "POST");
        assert_eq!(w.fields["authorization"], "Bearer ya29.refreshed");
        assert_eq!(w.fields["x-goog-user-project"], "quota-project-1");
        let body: serde_json::Value = serde_json::from_str(&w.body).unwrap();
        assert_eq!(body["logName"], "projects/my-project-1/logs/shards");
        assert_eq!(
            body["resource"],
            serde_json::json!({"type": "global", "labels": {"project_id": "my-project-1"}})
        );
        assert_eq!(body["partialSuccess"], true);
    }
    let stamped = shards(&["logs", "-t", "user"]);
    let mut by_line: Vec<(String, String)> = Vec::new();
    for e in &entries {
        let payload = &e["jsonPayload"];
        let line = payload["message"].as_str().unwrap().to_string();
        let container = &payload["container"];
        assert_eq!(container["name"], "/user");
        assert_eq!(container["id"], id);
        assert!(container["imageId"].as_str().unwrap().starts_with("sha256:"));
        assert!(
            container["command"].as_str().unwrap().contains("streams out err"),
            "{container}"
        );
        assert!(container["created"].as_str().is_some());
        let kept = [&stamped.stdout, &stamped.stderr]
            .iter()
            .flat_map(|s| s.lines())
            .find_map(|l| l.strip_suffix(&format!(" {line}")))
            .unwrap_or_else(|| panic!("{stamped}"));
        let kept = time::OffsetDateTime::parse(kept, &time::format_description::well_known::Rfc3339).unwrap();
        let sent = time::OffsetDateTime::parse(
            e["timestamp"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(kept, sent, "{e}");
        by_line.push((line, e["labels"]["stream"].as_str().unwrap().to_string()));
    }
    by_line.sort();
    assert_eq!(
        by_line,
        [
            ("err".to_string(), "stderr".to_string()),
            ("out".to_string(), "stdout".to_string())
        ]
    );

    // On GCE: its metadata server's project, instance and token; the log name the options
    // give.
    let ran = gcplogs(
        "instance",
        &["gcp-log-name=gcplogs-docker-driver"],
        &["streams", "a", "b"],
        &[("GCE_METADATA_HOST", &at)],
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let (asked, entries) = written(2);
    let w = asked.iter().find(|r| r.target == "/v2/entries:write").unwrap();
    assert_eq!(w.fields["authorization"], "Bearer ya29.instance");
    assert!(!w.fields.contains_key("x-goog-user-project"));
    let body: serde_json::Value = serde_json::from_str(&w.body).unwrap();
    assert_eq!(
        body["logName"],
        "projects/meta-project-1/logs/gcplogs-docker-driver"
    );
    assert_eq!(
        body["resource"],
        serde_json::json!({"type": "gce_instance", "labels": {"instance_id": "4242", "zone": "us-central1-a"}})
    );
    assert_eq!(
        entries[0]["jsonPayload"]["instance"],
        serde_json::json!({"zone": "us-central1-a", "name": "gce-vm", "id": "4242"})
    );
    let token = asked
        .iter()
        .find(|r| {
            r.target
                .starts_with("/computeMetadata/v1/instance/service-accounts/default/token")
        })
        .expect("the instance's token asked for");
    assert!(
        token
            .target
            .contains("scopes=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Flogging.write"),
        "{}",
        token.target
    );
    assert_eq!(token.fields["metadata-flavor"], "Google");

    for (opts, said) in [
        (&["tag=x"][..], "shards: --log-opt tag: gcplogs names each entry"),
        (
            &["gcp-project=Not_A_Project"],
            "shards: --log-opt gcp-project=Not_A_Project: a project's ID",
        ),
        (
            &["gcp-log-name=a b"],
            "shards: --log-opt gcp-log-name=a b: a log's name",
        ),
    ] {
        let r = gcplogs("refused", opts, &["exit", "0"], &[]);
        assert_eq!(r.status, Some(125), "{opts:?}: {r}");
        assert!(r.stderr.starts_with(said), "{opts:?}: {r}");
    }
}

/// A run's log as `--log-driver` and `--log-opt` keep it (D161): shards' store, its
/// segments as long and as many as asked; none with `none`, whose `logs` is refused and
/// whose attached client still gets its output; `--details` showing the labels, the
/// environment and the tag asked for; the refusals in shards' words; `inspect` as given.
#[test]
fn a_runs_log_is_kept_as_its_driver_and_options_say() {
    let Some((home, image)) = home("containers-logging") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let id_of = |name: &str| {
        shards(&["inspect", "-f", "{{.Id}}", name])
            .stdout
            .trim()
            .to_string()
    };
    // 10,000 lines through segments of 1,000 bytes, two kept: the newest, nothing more.
    let small = run_in(
        &home,
        &image,
        &[
            "--name",
            "small",
            "--log-driver",
            "json-file",
            "--log-opt",
            "max-size=1k",
            "--log-opt",
            "max-file=2",
        ],
        &["count", "10000"],
    );
    assert_eq!(small.status, Some(0), "{small}");
    let logged = shards(&["logs", "small"]);
    assert_eq!(logged.status, Some(0), "{logged}");
    assert!(
        logged.stdout.len() < 10_000 && logged.stdout.ends_with("line 10000\n"),
        "{} bytes, ending {:?}",
        logged.stdout.len(),
        logged.stdout.get(logged.stdout.len().saturating_sub(40)..)
    );
    assert!(
        small.stdout.ends_with(&logged.stdout),
        "the log is not the output's newest lines"
    );
    let dir = home.join("containers").join(id_of("small"));
    let segments = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("log") && !n.ends_with(".idx"))
        .count();
    assert_eq!(segments, 2, "{}", dir.display());
    let shown = shards(&[
        "inspect",
        "-f",
        "{{.HostConfig.LogConfig.Type}} {{json .HostConfig.LogConfig.Config}}",
        "small",
    ]);
    assert_eq!(
        shown.stdout, "json-file {\"max-file\":\"2\",\"max-size\":\"1k\"}\n",
        "{shown}"
    );
    // None kept: its output its attached client's alone.
    let quiet = run_in(
        &home,
        &image,
        &["--name", "quiet", "--log-driver", "none"],
        &["streams", "out", "err"],
    );
    assert_eq!(
        (quiet.status, quiet.stdout.as_str()),
        (Some(0), "out\n"),
        "{quiet}"
    );
    let quiet_dir = home.join("containers").join(id_of("quiet"));
    let written: u64 = std::fs::read_dir(&quiet_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("log"))
        .map(|e| e.metadata().map_or(0, |m| m.len()))
        .sum();
    assert_eq!(written, 0, "{}", quiet_dir.display());
    let refused = shards(&["logs", "quiet"]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (
            Some(1),
            "shards: quiet keeps no log: it was run with --log-driver none\n"
        ),
        "{refused}"
    );
    // Details: the labels, the environment and the tag asked for, sorted, before each line.
    let tagged = run_in(
        &home,
        &image,
        &[
            "--name",
            "tagged",
            "--label",
            "app=web",
            "-e",
            "E=one two",
            "--log-opt",
            "labels=app",
            "--log-opt",
            "env=E",
            "--log-opt",
            "tag={{.Name}}",
        ],
        &["streams", "out", "err"],
    );
    assert_eq!(tagged.status, Some(0), "{tagged}");
    let details = shards(&["logs", "--details", "tagged"]);
    assert_eq!(
        (details.stdout.as_str(), details.stderr.as_str()),
        (
            "E=one+two,app=web,tag=tagged out\n",
            "E=one+two,app=web,tag=tagged err\n"
        ),
        "{details}"
    );
    // Another OS's own log, which this host keeps none of: Windows sends a run's to ETW
    // (D198), macOS to its unified log (D199).
    let (foreign, foreign_said) = if cfg!(windows) {
        (
            "oslog",
            "shards: --log-driver oslog: the unified log is macOS's own",
        )
    } else {
        (
            "etwlogs",
            "shards: --log-driver etwlogs: Event Tracing for Windows is Windows' own",
        )
    };
    for (flags, said) in [
        (&["--log-driver", foreign][..], foreign_said),
        (
            &["--log-driver", "bogus"],
            "shards: --log-driver bogus: no such log driver",
        ),
        (
            &["--log-opt", "max-file=0"],
            "shards: --log-opt max-file=0: how many files, at least 1",
        ),
        (
            &["--log-opt", "color=red"],
            "shards: --log-opt color: --log-driver shards takes max-size",
        ),
        (
            &["--log-driver", "none", "--log-opt", "max-size=1m"],
            "shards: --log-opt with --log-driver none: a run with no log takes no log options",
        ),
    ] {
        let r = run_in(&home, &image, flags, &["exit", "0"]);
        assert_eq!(r.status, Some(125), "{flags:?}: {r}");
        assert!(r.stderr.starts_with(said), "{flags:?}: {r}");
    }
}
