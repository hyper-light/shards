//! A microVM's members as a command run in them finds them (the harness plan's step 6,
//! G24; D243), on real VMs booted from SHARDS_KERNEL and SHARDS_INIT: an image from a
//! loopback registry (tests/common, `served`) whose Agentfile's members are the test guest.
//! What a member sees of itself is what its `confined` mode says on its stdout, read as its
//! owner reads it (D180): booted for an orchestrator through shards-api's `ImageShards` and
//! this build's `shards api-run` (D231), as a daemon's run hands a member's output to its
//! owner's endpoint alone, never to the run's own stderr. A command run in a member with
//! `shards exec vm --for` says the same on the exec's own stdout. A member on a terminal of
//! its own (D244) is read, sized and typed into as its owner does.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader};
use std::time::Duration;

use common::terminal::Terminal;
use common::{Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use shards_api::image::{
    Channel, ImageGuest, ImageShards, ImageSpec, Images as _, MemberName, MemberSpec, Narrowing, RunEnd,
};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The variables a test's run host is given: its home, this build's guest, and the tests'
/// own docker config and local store (`common::as_tests_run`'s).
fn host_env(home: &TempDir) -> Vec<(OsString, OsString)> {
    vec![
        ("SHARDS_HOME".into(), home.as_os_str().into()),
        ("SHARDS_KERNEL".into(), kernel().as_os_str().into()),
        ("SHARDS_INIT".into(), guest_init().as_os_str().into()),
        ("SHARDS_LOCAL_STORE".into(), "none".into()),
        ("DOCKER_CONFIG".into(), common::docker_config().as_os_str().into()),
    ]
}

fn codex() -> MemberName {
    MemberName::harness("codex")
}

/// Builds an image whose harness `codex` is the test guest started with `options`, tagged
/// `tag` in `home`.
fn build(home: &TempDir, tag: &str, options: &[&str]) {
    let (image, _) = served();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ctx = TempDir::new("mx-ctx");
    let binary = std::fs::read(common::test_guest()).unwrap();
    let options: Vec<String> = options.iter().map(|o| format!("{o:?}")).collect();
    let descriptor = format!(r#"{{"entrypoint":"codex","options":[{}]}}"#, options.join(","));
    std::fs::write(
        ctx.join("harness.tar"),
        common::tar(&[
            ("bin", 0o755, 0, None),
            ("bin/codex", 0o755, 0, Some(&binary)),
            ("harness.json", 0o644, 0, Some(descriptor.as_bytes())),
        ]),
    )
    .unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nHARNESS codex FROM ./harness.tar\n"),
    )
    .unwrap();
    let built = run_shards_env(
        &["build", "image"],
        &["-t", tag, ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
}

/// `image` booted from `home` with its harness running as the image declares it.
fn boot(home: &TempDir, image: &str) -> Box<dyn ImageGuest> {
    boot_member(home, image, false, false)
}

/// `image` booted from `home`, its harness given a terminal of its own and its stdin as
/// asked (D244).
fn boot_member(home: &TempDir, image: &str, terminal: bool, stdin: bool) -> Box<dyn ImageGuest> {
    let spec = ImageSpec {
        image: image.into(),
        vcpus: 1,
        memory_mib: 512,
        command: None,
        members: vec![MemberSpec {
            member: codex(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            stdin,
            terminal,
            args: Vec::new(),
        }],
        publish: Vec::new(),
        egress: Narrowing::default(),
        credentials: Vec::new(),
        lessor: None,
        shares: Vec::new(),
        nesting: false,
        ready_by: None,
        binary: Some(common::shards().to_path_buf()),
        env: host_env(home),
        control: None,
        gpu_devices: Vec::new(),
        sign_ins: Vec::new(),
    };
    ImageShards::new().boot(&spec, TIMEOUT).unwrap()
}

/// The lines the harness writes on its stdout up to the first that begins with `last`,
/// that one included: `confined` says it all at once, then waits to be ended.
fn lines_until(guest: &dyn ImageGuest, last: &str) -> Vec<String> {
    let pipe = guest.take_output(&codex(), Channel::Stdout).unwrap();
    let last = last.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut said = Vec::new();
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            let done = line.starts_with(&last);
            said.push(line);
            if done {
                break;
            }
        }
        let _ = tx.send(said);
    });
    rx.recv_timeout(TIMEOUT)
        .expect("the harness said nothing in time")
}

/// A member's process starts with its signals as an exec's does (shards-init's
/// `defaults::signals`): none ignored, though init ignores SIGPIPE (Rust's runtime does) and
/// execve keeps an ignored signal ignored, and none blocked. So a process of the member's
/// writing to a pipe whose reader has gone dies of SIGPIPE, as one a shell starts does,
/// where it saw EPIPE: the probe's child, which keeps the disposition the probe was given.
#[test]
fn a_member_starts_with_every_signal_at_its_default() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("mx-signals-home");
    build(&home, "mx-signals:1", &["confined", "signals", "pipe"]);
    let guest = boot(&home, "mx-signals:1");
    let said = lines_until(&*guest, "confined net=");
    for want in [
        "confined signals ignored=0000000000000000 blocked=0000000000000000",
        "confined pipe: signal 13",
    ] {
        assert!(
            said.iter().any(|l| l == want),
            "no {want:?} in:\n{}",
            said.join("\n")
        );
    }
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}

/// `shards ARGS` in `home`, with this build's guest.
fn shards_in(home: &TempDir, args: &[&str]) -> Run {
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    run_shards_env(&[], args, &env, TIMEOUT)
}

/// Runs, detached as `name`, an image whose agent `other` (member 0, uid 200000) holds a
/// file of its own, `secret`, and whose harness `codex` (member 1, uid 200001) is the test
/// guest too, each waiting to be ended, as the run's own command does.
fn run_two(home: &TempDir, name: &str) {
    let (image, _) = served();
    let ctx = TempDir::new("mx-two-ctx");
    let binary = std::fs::read(common::test_guest()).unwrap();
    std::fs::create_dir_all(ctx.join("other/bin")).unwrap();
    std::fs::write(ctx.join("other/bin/testguest"), &binary).unwrap();
    // A context's files keep their modes on a Unix host, as fsutil walks them, and a
    // written file is no program (0644 under the usual umask): its member's launch would
    // be refused (EACCES). A Windows host's context gives every file 0755.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            ctx.join("other/bin/testguest"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    std::fs::write(ctx.join("other/secret"), "the other member's\n").unwrap();
    std::fs::write(
        ctx.join("other/agent.json"),
        r#"{"entrypoint":"bin/testguest","options":["sleep"]}"#,
    )
    .unwrap();
    std::fs::write(
        ctx.join("harness.tar"),
        common::tar(&[
            ("bin", 0o755, 0, None),
            ("bin/codex", 0o755, 0, Some(&binary)),
            (
                "harness.json",
                0o644,
                0,
                Some(br#"{"entrypoint":"codex","options":["sleep"]}"#),
            ),
        ]),
    )
    .unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT other FROM ./other\nHARNESS codex FROM ./harness.tar\n"),
    )
    .unwrap();
    let tag = format!("{name}:1");
    let built = shards_in(home, &["build", "image", "-t", &tag, ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{built}");
    let ran = shards_in(home, &["run", "vm", "-d", "--name", name, &tag, "sleep"]);
    assert_eq!(ran.status, Some(0), "{ran}");
}

/// The lines a command said that `confined` makes.
fn confined(run: &Run) -> Vec<&str> {
    run.stdout
        .lines()
        .filter(|l| l.starts_with("confined "))
        .collect()
}

/// A command run in a member with `exec --for` runs as that member does (the member
/// contract tests/build.rs's `confined` holds a member to): its uid and no other group, no
/// capability and no_new_privs, its Landlock rules on files and TCP, its seccomp filter, its
/// signals at their defaults, its own name, network and cgroup, its member's PID namespace.
/// What the member cannot reach it cannot: another member's files, by path or by a link it
/// plants, and the other's processes, which are not there to see; and it holds nothing of
/// init's but its stdio. Each exec lands in the member it names.
#[test]
fn a_command_run_in_a_member_is_confined_as_the_member_is() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("mx-in-home");
    run_two(&home, "mx-in");
    let probe = [
        "confined",
        "signals",
        "see",
        "/agents/other",
        "/sys",
        "/proc/self/fd",
        "write",
        "/etc/x",
        "/tmp/x",
        "/tmp/../proc/self/comm",
        "bind",
        "127.0.0.1:8080",
        "connect",
        "127.0.0.1:9",
        "call",
        "socket-vsock",
        "socket-netlink",
        "keyctl",
        "userfaultfd",
        "clone3",
        "fork",
        "follow",
        "/tmp/l=/agents/other/secret",
        "hardlink",
        "/tmp/h=/agents/other/secret",
        "cat",
        "/proc/self/cgroup",
        "done",
    ];
    let mut args = vec!["exec", "vm", "--for", "codex", "mx-in", "codex"];
    args.extend(probe);
    let ran = shards_in(&home, &args);
    assert_eq!(ran.status, Some(0), "{ran}");
    let said = confined(&ran);
    for want in [
        "confined uid=200001 gid=200001 groups=0",
        // Its member's PID namespace: the member's reaper and command, and itself.
        "confined pid=3 procs=3",
        "confined hostname=harness-codex",
        "confined caps eff=0000000000000000 prm=0000000000000000 inh=0000000000000000 bnd=0000000000000000 amb=0000000000000000 nnp=1",
        "confined signals ignored=0000000000000000 blocked=0000000000000000",
        "confined see /agents/other: errno 13",
        "confined see /sys: errno 13",
        // Its stdio and the directory being listed: nothing of init's.
        "confined see /proc/self/fd: 0,1,2,3",
        "confined write /etc/x: errno 30",
        "confined write /tmp/x: ok",
        // Landlock: no write outside its scratch and /dev, and no TCP.
        "confined write /tmp/../proc/self/comm: errno 13",
        "confined bind 127.0.0.1:8080: errno 13",
        "confined connect 127.0.0.1:9: errno 13",
        // seccomp: the member's filter.
        "confined call socket-vsock: errno 1",
        "confined call socket-netlink: errno 1",
        "confined call keyctl: errno 1",
        "confined call userfaultfd: errno 1",
        "confined call clone3: errno 38",
        "confined call fork: ok",
        "confined follow /tmp/l=/agents/other/secret: errno 13",
        "confined hardlink /tmp/h=/agents/other/secret: errno 13",
        // Its member's cgroup, the root of its cgroup namespace.
        "confined cat /proc/self/cgroup: 0::/",
        "confined net=lo",
    ] {
        assert!(said.contains(&want), "no {want:?} in:\n{}", ran.stdout);
    }
    // The other member, entered the same way, is itself.
    let other = shards_in(
        &home,
        &[
            "exec",
            "vm",
            "--for",
            "other",
            "mx-in",
            "testguest",
            "confined",
            "done",
        ],
    );
    assert_eq!(other.status, Some(0), "{other}");
    let said = confined(&other);
    for want in [
        "confined uid=200000 gid=200000 groups=0",
        "confined hostname=agent-other",
    ] {
        assert!(said.contains(&want), "no {want:?} in:\n{}", other.stdout);
    }
    let _ = shards_in(&home, &["remove", "vm", "-f", "mx-in"]);
}

/// What `exec --for` cannot do is refused by type, each in words of its own: a member of no
/// such name; a user or privileges of its own, which a member's command does not have; a
/// program its member's `PATH` does not hold, as `docker exec` says it.
#[test]
fn what_a_member_has_not_is_refused_to_a_command_run_in_it() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("mx-no-home");
    run_two(&home, "mx-no");
    for (args, status, words) in [
        (
            &["exec", "vm", "--for", "nosuch", "mx-no", "codex"][..],
            1,
            "--for nosuch: no agent or harness of its image is named so",
        ),
        (
            &["exec", "vm", "--for", "codex", "-u", "0", "mx-no", "codex"],
            1,
            "--for codex: a command run in it runs as it does, so takes no --user",
        ),
        (
            &["exec", "vm", "--for", "codex", "--privileged", "mx-no", "codex"],
            1,
            "--for codex: a command run in it is confined as it is, so not --privileged",
        ),
        (
            &["exec", "vm", "--for", "codex", "mx-no", "nosuch-program"],
            127,
            "executable file not found in $PATH",
        ),
    ] {
        let ran = shards_in(&home, args);
        assert_eq!(ran.status, Some(status), "{args:?}: {ran}");
        assert!(ran.stderr.contains(words), "{args:?}: {ran}");
    }
    let _ = shards_in(&home, &["remove", "vm", "-f", "mx-no"]);
}

/// A command run in a member with a terminal: its stdio a pty, its session's controlling
/// terminal with it in the foreground, sized as the client's and resized as the client's
/// is; and it ends with its status, the client's terminal as it was.
#[test]
fn a_command_run_in_a_member_has_a_terminal_that_resizes() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("mx-tty-home");
    run_two(&home, "mx-tty");
    let mut term = Terminal::run(
        &home,
        &[
            "exec", "vm", "-it", "--for", "codex", "mx-tty", "codex", "tty", "winch",
        ],
        24,
        80,
    );
    let shown = term.wait_for("ready\r\n");
    for want in [
        "stdin tty true\r\n",
        "stdout tty true\r\n",
        "controlling true\r\nforeground true\r\n",
        "size 24 80\r\n",
    ] {
        assert!(shown.contains(want), "no {want:?} in {shown}");
    }
    term.resize(40, 120);
    term.wait_for("resized 40 120\r\n");
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true));
    let _ = shards_in(&home, &["remove", "vm", "-f", "mx-tty"]);
}

/// The lines the harness's terminal shows on its stdout, each as it comes, without the
/// carriage return a terminal's line discipline puts before its newline (termios(3),
/// ONLCR).
fn terminal_lines(guest: &dyn ImageGuest) -> std::sync::mpsc::Receiver<String> {
    let pipe = guest.take_output(&codex(), Channel::Stdout).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            if tx.send(line.trim_end_matches('\r').to_string()).is_err() {
                break;
            }
        }
    });
    rx
}

/// The lines `lines` gives up to the first that begins with `last`, that one included;
/// where none comes, how the harness and the run ended is said beside what came: its end
/// waited for where its terminal closed, which it follows.
fn until(guest: &dyn ImageGuest, lines: &std::sync::mpsc::Receiver<String>, last: &str) -> Vec<String> {
    let mut said = Vec::new();
    loop {
        let line = lines.recv_timeout(TIMEOUT).unwrap_or_else(|e| {
            let closed = matches!(e, std::sync::mpsc::RecvTimeoutError::Disconnected);
            panic!(
                "no {last:?} in time ({e}) after:\n{}\nthe harness's end: {:?}; the run's: {:?}",
                said.join("\n"),
                guest.wait_member(&codex(), if closed { TIMEOUT } else { Duration::ZERO }),
                guest.wait_for(Duration::ZERO)
            )
        });
        let done = line.starts_with(last);
        said.push(line);
        if done {
            return said;
        }
    }
}

/// A harness whose spec asks for a terminal of its own (plan step 6, D244): its stdin,
/// stdout and stderr are that terminal, its session's controlling terminal with it in the
/// foreground, 0 by 0 as the kernel makes one. Its owner reads what the terminal shows as
/// its stdout, its stderr ending at once, and sizes it, which the harness hears as SIGWINCH.
#[test]
fn a_harness_on_a_terminal_of_its_own_is_shown_and_sized_by_its_owner() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("mx-term-home");
    build(&home, "mx-term:1", &["tty", "winch"]);
    let guest = boot_member(&home, "mx-term:1", true, false);
    let mut stderr = guest.take_output(&codex(), Channel::Stderr).unwrap();
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut stderr, &mut rest).unwrap();
    assert!(rest.is_empty(), "its stderr is its terminal: {rest:?}");
    let lines = terminal_lines(&*guest);
    let said = until(&*guest, &lines, "ready");
    for want in [
        "stdin tty true",
        "stdout tty true",
        "stderr tty true",
        "controlling true",
        "foreground true",
        "size 0 0",
    ] {
        assert!(said.iter().any(|l| l == want), "no {want:?} in {said:?}");
    }
    guest.resize(&codex(), 40, 120, TIMEOUT).unwrap();
    let said = until(&*guest, &lines, "resized");
    assert_eq!(said.last().map(String::as_str), Some("resized 40 120"));
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}

/// What a harness's owner writes to its stdin is typed into its terminal (D244): the
/// terminal echoes the line, and the harness reads it as typed.
#[test]
fn a_harness_on_a_terminal_of_its_own_reads_what_its_owner_types() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("mx-keys-home");
    build(&home, "mx-keys:1", &["tty", "read"]);
    let guest = boot_member(&home, "mx-keys:1", true, true);
    let lines = terminal_lines(&*guest);
    until(&*guest, &lines, "ready");
    let mut keys = guest.take_input(&codex()).unwrap();
    std::io::Write::write_all(&mut keys, b"hello\n").unwrap();
    let said = until(&*guest, &lines, "read ");
    assert_eq!(said.last().map(String::as_str), Some("read hello"));
    assert!(
        said.iter().any(|l| l == "hello"),
        "the terminal echoes what is typed: {said:?}"
    );
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}
