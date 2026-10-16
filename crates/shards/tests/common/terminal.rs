//! The user's terminal, for E2E tests of runs on one: a pty the test holds, whose peer is
//! the client's terminal (tests/tty.rs, tests/context.rs).

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{guest_init, kernel, shards};

const TIMEOUT: Duration = Duration::from_secs(60);

/// The user's terminal: a pty whose peer is a shell's controlling terminal, the session's
/// leader, which runs the client in its foreground group as a login shell would, so that
/// the terminal outlives the client: XNU revokes a controlling terminal when its session
/// leader exits (bsd/kern/kern_exit.c, proc_exit: ttywait, then VNOP_REVOKE). The shell
/// prints the client's status, then waits for a line; the test reads the master, and
/// types into it.
pub struct Terminal {
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
    pub fn run(home: &Path, args: &[&str], rows: u16, cols: u16) -> Terminal {
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
    pub fn wait_for(&mut self, text: &str) -> String {
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

    pub fn type_in(&mut self, keys: &[u8]) {
        self.master.write_all(keys).unwrap();
    }

    /// Resizes the terminal, as a window does: the kernel signals its foreground.
    pub fn resize(&self, rows: u16, cols: u16) {
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
    pub fn settings(&self) -> libc::termios {
        settings(&self.peer)
    }

    /// Once the client exits: what it showed after the last wait, its status, and
    /// whether it left the terminal's settings as it found them. XNU sets PENDIN itself
    /// when a terminal goes back to canonical mode, to retype pending input (bsd/kern/
    /// tty.c, ttioctl): a state, not a setting.
    pub fn exit(&mut self) -> (String, i32, bool) {
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
pub fn settings(fd: &OwnedFd) -> libc::termios {
    // SAFETY: an all-zero termios is a valid out-parameter for tcgetattr(3).
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let got = unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) };
    assert_eq!(got, 0, "tcgetattr: {}", std::io::Error::last_os_error());
    t
}

/// The settings raw mode changes (moby/term termios_unix.go).
pub fn cooked(t: &libc::termios) -> (bool, bool, bool) {
    (
        t.c_lflag & libc::ICANON != 0,
        t.c_lflag & libc::ECHO != 0,
        t.c_oflag & libc::OPOST != 0,
    )
}
