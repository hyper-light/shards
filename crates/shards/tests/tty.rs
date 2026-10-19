//! `shards run -t` and `-it`, end to end, as `docker run` has them
//! (docs/research/tty-and-interactive-runs.md): the command's stdio is a pty in the guest,
//! and under `-it` the client's terminal goes raw, detaches on its keys, and resizes the
//! pty. Real VMs, booted from SHARDS_KERNEL and SHARDS_INIT; the image comes from a
//! loopback registry (tests/common, `served`), and its entrypoint is the test guest, whose
//! `tty` mode reports what the command sees. The client runs on a pty of the test's, which
//! plays the user's terminal.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use common::terminal::{Terminal, cooked};
use common::{Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(60);

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

#[test]
fn a_tty_is_the_commands_stdio_and_its_session() {
    let Some((home, image)) = home("tty-stdio") else {
        return;
    };
    let run = |options: &[&str]| {
        let mut args = vec!["run", "--pull", "never"];
        args.extend_from_slice(options);
        args.extend([image.as_str(), "tty"]);
        shards_in(&home, &args)
    };
    // Stdout is not a terminal here, so the pty keeps the kernel's 0×0.
    let t = run(&["-t"]);
    assert_eq!(t.status, Some(0), "{}", t.stderr);
    assert_eq!(
        t.stdout,
        "stdin tty true\r\nstdout tty true\r\nstderr tty true\r\ncontrolling true\r\n\
         foreground true\r\nsize 0 0\r\nterm xterm\r\nready\r\n"
    );
    let term = run(&["-t", "-e", "TERM=vt100"]);
    assert!(term.stdout.contains("term vt100\r\n"), "{}", term.stdout);
    let piped = run(&[]);
    assert!(
        piped.stdout.starts_with("stdin tty false\nstdout tty false\n"),
        "{}",
        piped.stdout
    );
    assert!(piped.stdout.contains("\nterm \n"), "{}", piped.stdout);

    // What a terminal's command wrote is logged as it came, line ends and all.
    let named = run(&["-t", "--name", "logged"]);
    assert_eq!(named.status, Some(0), "{}", named.stderr);
    let logs = shards_in(&home, &["logs", "logged"]);
    assert_eq!(logs.stdout, named.stdout, "{}", logs.stderr);
}

/// A command with a terminal is told its client's terminal type where its image describes
/// it (a terminfo entry), and xterm where not; a client whose terminal speaks 24-bit colour,
/// or 256 colours by its own description on the host, xterm-256color where that is
/// described. The run's own TERM is its own. An exec chooses for its own client.
#[test]
fn a_terminal_is_told_its_clients_type_where_its_image_describes_it() {
    let Some((home, image)) = home("tty-term") else {
        return;
    };
    // An image describing xterm-256color, and a terminal of the test's own.
    let ctx = TempDir::new("tty-term-ctx");
    std::fs::write(
        ctx.join("Dockerfile"),
        format!("FROM {image}\nCOPY terminfo /usr/share/terminfo\n"),
    )
    .unwrap();
    for entry in ["x/xterm-256color", "s/shards-test-term"] {
        let at = ctx.join("terminfo").join(entry);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, b"an entry").unwrap();
    }
    let built = shards_in(
        &home,
        &["build", "-q", "-t", "described:1", ctx.to_str().unwrap()],
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let (kernel, init) = (kernel(), guest_init());
    let told = |term: &str, colorterm: &str, args: &[&str]| {
        let env: [(&str, &OsStr); 5] = [
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel.as_os_str()),
            ("SHARDS_INIT", init.as_os_str()),
            ("TERM", term.as_ref()),
            ("COLORTERM", colorterm.as_ref()),
        ];
        let r = run_shards_env(&[], args, &env, TIMEOUT);
        assert_eq!(r.status, Some(0), "{args:?}: {}", r.stderr);
        r.stdout
            .lines()
            .find_map(|l| l.strip_prefix("term "))
            .unwrap_or_default()
            .trim_end()
            .to_string()
    };
    let run = |term: &str, colorterm: &str, image: &str, options: &[&str]| {
        let mut args = vec!["run", "--rm", "--pull", "never", "-t"];
        args.extend_from_slice(options);
        args.extend([image, "tty"]);
        told(term, colorterm, &args)
    };
    assert_eq!(
        run("shards-test-term", "", "described:1", &[]),
        "shards-test-term"
    );
    assert_eq!(run("xterm-256color", "", "described:1", &[]), "xterm-256color");
    assert_eq!(run("no-such-term", "", "described:1", &[]), "xterm");
    assert_eq!(
        run("no-such-term", "truecolor", "described:1", &[]),
        "xterm-256color"
    );
    assert_eq!(
        run("shards-test-term", "", "described:1", &["-e", "TERM=vt100"]),
        "vt100"
    );
    // An image that describes no terminal: xterm, whatever the client's.
    assert_eq!(run("xterm-256color", "truecolor", &image, &[]), "xterm");
    // An exec, from a client of its own.
    let held = shards_in(
        &home,
        &[
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            "termed",
            "described:1",
            "sleep",
        ],
    );
    assert_eq!(held.status, Some(0), "{}", held.stderr);
    let exec = ["exec", "-t", "termed", "/bin/testguest", "tty"];
    assert_eq!(told("shards-test-term", "", &exec), "shards-test-term");
    assert_eq!(told("no-such-term", "", &exec), "xterm");
    assert_eq!(shards_in(&home, &["rm", "-f", "termed"]).status, Some(0));
}

#[test]
fn a_tty_for_stdin_needs_a_terminal_and_detach_keys_must_parse() {
    let Some((home, image)) = home("tty-refused") else {
        return;
    };
    let refused = shards_in(&home, &["run", "--pull", "never", "-it", &image, "tty"]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (
            Some(1),
            "-it: stdin is not a terminal, so it cannot be the command's: leave out -t, or run it from a terminal\n"
        )
    );
    let keys = shards_in(
        &home,
        &["run", "--pull", "never", "--detach-keys", "ctrl-P", &image, "tty"],
    );
    assert_eq!(
        (keys.status, keys.stderr.as_str()),
        (
            Some(1),
            "invalid detach keys (ctrl-P): Unknown character: 'ctrl-P'\n"
        )
    );
    let listed = shards_in(&home, &["ps", "-a", "-q"]);
    assert_eq!(listed.stdout, "", "nothing was created");
}

#[test]
fn an_interactive_tty_is_raw_sized_and_restored() {
    let Some((home, image)) = home("tty-raw") else {
        return;
    };
    let mut term = Terminal::run(
        &home,
        &["run", "--pull", "never", "-it", &image, "tty", "read"],
        30,
        100,
    );
    let shown = term.wait_for("ready\r\n");
    assert!(shown.contains("stdin tty true\r\n"), "{shown}");
    assert!(
        shown.contains("controlling true\r\nforeground true\r\n"),
        "{shown}"
    );
    assert!(shown.contains("size 30 100\r\n"), "{shown}");
    assert_eq!(
        cooked(&term.settings()),
        (false, false, false),
        "raw while it runs"
    );
    // Return is a carriage return in raw mode; the guest's pty turns it into a newline,
    // and echoes what was typed.
    term.type_in(b"hello\r");
    let shown = term.wait_for("read hello\r\n");
    assert!(shown.contains("hello\r\n"), "echoed: {shown}");
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true));
}

#[test]
fn a_resize_reaches_the_command() {
    let Some((home, image)) = home("tty-resize") else {
        return;
    };
    let mut term = Terminal::run(
        &home,
        &["run", "--pull", "never", "-it", &image, "tty", "winch"],
        24,
        80,
    );
    let shown = term.wait_for("ready\r\n");
    assert!(shown.contains("size 24 80\r\n"), "{shown}");
    term.resize(40, 120);
    term.wait_for("resized 40 120\r\n");
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (0, true));
}

#[test]
fn ctrl_c_interrupts_and_the_detach_keys_leave_it_running() {
    let Some((home, image)) = home("tty-keys") else {
        return;
    };
    // ^C reaches the command as SIGINT from its own terminal: the client's is raw. Under
    // --init, where the command, which does not handle SIGINT, is not PID 1, which would
    // not hear it, as under `docker run -it` (Docker 29.3.1: exit 130 with `--init`; with
    // none, the ^C unheard, PM M145).
    let mut term = Terminal::run(
        &home,
        &["run", "--pull", "never", "-it", "--init", &image, "sleep"],
        24,
        80,
    );
    term.wait_for("ready\r\n");
    term.type_in(&[3]);
    let (_, status, restored) = term.exit();
    assert_eq!((status, restored), (128 + libc::SIGINT, true));

    // The detach keys end the client, which says nothing and exits 0, and the command
    // runs on; custom keys likewise.
    for (options, keys) in [
        (vec![], vec![16u8, 17]),
        (vec!["--detach-keys", "ctrl-a,d"], vec![1, b'd']),
    ] {
        let mut args = vec!["run", "--pull", "never", "-it", "--name", "left"];
        args.extend(options.iter().copied());
        args.extend([image.as_str(), "sleep"]);
        let mut term = Terminal::run(&home, &args, 24, 80);
        term.wait_for("ready\r\n");
        term.type_in(&keys);
        let (shown, status, restored) = term.exit();
        assert_eq!((shown.as_str(), status, restored), ("", 0, true));
        let listed = shards_in(&home, &["ps"]);
        assert!(listed.stdout.contains(" left\n"), "{}", listed.stdout);
        let removed = shards_in(&home, &["rm", "-f", "left"]);
        assert_eq!(removed.status, Some(0), "{}", removed.stderr);
    }
}
