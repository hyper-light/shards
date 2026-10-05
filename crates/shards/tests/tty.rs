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
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served, shards};

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

/// The user's terminal: a pty whose peer is a shell's controlling terminal, the session's
/// leader, which runs the client in its foreground group as a login shell would, so that
/// the terminal outlives the client: XNU revokes a controlling terminal when its session
/// leader exits (bsd/kern/kern_exit.c, proc_exit: ttywait, then VNOP_REVOKE). The shell
/// prints the client's status, then waits for a line; the test reads the master, and
/// types into it.
struct Terminal {
    master: File,
    /// The peer, kept to read its settings.
    peer: OwnedFd,
    shell: Child,
    seen: String,
    /// The settings before the client ran.
    before: libc::termios,
}

impl Terminal {
    /// `shards ARGS` in `home` on a terminal of `rows` by `cols`.
    fn run(home: &Path, args: &[&str], rows: u16, cols: u16) -> Terminal {
        let (mut master, mut peer) = (0, 0);
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty(3) fills both descriptors and reads `ws`.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut peer,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                (&raw const ws).cast_mut(),
            )
        };
        assert_eq!(opened, 0);
        // SAFETY: fresh descriptors nothing else owns.
        let (master, peer) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(peer)) };
        let before = settings(&peer);
        let stdio = || Stdio::from(peer.try_clone().unwrap());
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(r#""$0" "$@"; echo "exit $?"; read done"#)
            .arg(shards())
            .args(args)
            .env("SHARDS_HOME", home)
            .env("SHARDS_LOCAL_STORE", "none")
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .env_remove("NORAW")
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio());
        // SAFETY: runs in the child between fork and exec, after its stdio is the peer,
        // calling only setsid(2) and ioctl(2), which are async-signal-safe: the shell
        // leads a session whose controlling terminal is the peer.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let shell = command.spawn().unwrap();
        // SAFETY: fcntl(2) on our own descriptor.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Terminal {
            master,
            peer,
            shell,
            seen: String::new(),
            before,
        }
    }

    /// Reads what the terminal shows until `text` has appeared since the last wait, and
    /// returns everything read since then.
    fn wait_for(&mut self, text: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        let mut buf = [0u8; 4096];
        loop {
            if let Some(at) = self.seen.find(text) {
                let upto = at + text.len();
                let read: String = self.seen.drain(..upto).collect();
                return read;
            }
            match self.master.read(&mut buf) {
                Ok(n) if n > 0 => self.seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                _ => {
                    assert!(
                        Instant::now() < deadline,
                        "{text:?} did not come; the terminal showed {:?}",
                        self.seen
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn type_in(&mut self, keys: &[u8]) {
        self.master.write_all(keys).unwrap();
    }

    /// Resizes the terminal, as a window does: the kernel signals its foreground.
    fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads one winsize.
        let set = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        assert_eq!(set, 0);
    }

    /// The terminal's settings now.
    fn settings(&self) -> libc::termios {
        settings(&self.peer)
    }

    /// Once the client exits: what it showed after the last wait, its status, and
    /// whether it left the terminal's settings as it found them. XNU sets PENDIN itself
    /// when a terminal goes back to canonical mode, to retype pending input (bsd/kern/
    /// tty.c, ttioctl): a state, not a setting.
    fn exit(&mut self) -> (String, i32, bool) {
        let shown = self.wait_for("exit ");
        let shown = shown.strip_suffix("exit ").unwrap_or_default().to_string();
        let status = self.wait_for("\r\n");
        let (a, b) = (self.settings(), self.before);
        let restored = a.c_iflag == b.c_iflag
            && a.c_oflag == b.c_oflag
            && a.c_cflag == b.c_cflag
            && (a.c_lflag & !libc::PENDIN) == (b.c_lflag & !libc::PENDIN)
            && a.c_cc == b.c_cc;
        // The shell's exit waits for its terminal's output to drain (ttywait, above), so
        // the master is read meanwhile.
        self.type_in(b"\n");
        let deadline = Instant::now() + TIMEOUT;
        let mut buf = [0u8; 4096];
        while self.shell.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the shell did not exit");
            let _ = self.master.read(&mut buf);
            std::thread::sleep(Duration::from_millis(5));
        }
        (shown, status.trim().parse().unwrap(), restored)
    }
}

/// The settings of the terminal `fd` is.
fn settings(fd: &OwnedFd) -> libc::termios {
    // SAFETY: an all-zero termios is a valid out-parameter for tcgetattr(3).
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let got = unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) };
    assert_eq!(got, 0, "tcgetattr: {}", std::io::Error::last_os_error());
    t
}

/// The settings raw mode changes (moby/term termios_unix.go).
fn cooked(t: &libc::termios) -> (bool, bool, bool) {
    (
        t.c_lflag & libc::ICANON != 0,
        t.c_lflag & libc::ECHO != 0,
        t.c_oflag & libc::OPOST != 0,
    )
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
            "cannot attach stdin to a TTY-enabled container because stdin is not a terminal\n"
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
    // ^C reaches the command as SIGINT from its own terminal: the client's is raw.
    let mut term = Terminal::run(&home, &["run", "--pull", "never", "-it", &image, "sleep"], 24, 80);
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
