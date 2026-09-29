//! The client side of `shards run` (docs/design/architecture.md D26). It asks the daemon
//! for the run, starting the daemon if there is none, and passes it this process's stdout
//! and stderr, which the command then writes to itself. The command's stdin is /dev/null,
//! or with `-i` a pipe the client fills from its own stdin: then the command's stdin ends
//! when the client does, as `docker run -i`'s does when its client goes (StdinOnce), and
//! only the client reads its terminal. The client forwards the signals it gets, as
//! `docker run` does, and exits with the command's status.
//!
//! It needs only `shards_ipc` and the OS: it is the thin `shards` binary's (main.rs).

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_ipc::{Command, Run, SOCKET, kind, log};

use crate::NOT_RUN;

/// How long a started daemon may take to listen: longer than the one it replaces may take
/// to end its runs and exit (daemon.rs, TAKEOVER).
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `request` through the daemon of `home`, whose binary is `daemon`. Every path the
/// request holds is absolute by now (request.rs), since this process makes the home its
/// working directory, where the daemon's socket is (`shards_ipc::SOCKET`).
pub fn run(home: &Path, daemon: &Path, request: &Run) -> ExitCode {
    // Before any thread starts, none of which may use a relative path meanwhile.
    let mut started = match enter(home, daemon) {
        Ok(started) => started,
        Err(e) => return failed(&e),
    };
    // SAFETY: isatty(3) on this process's stdin.
    let reads_terminal = request.interactive && unsafe { libc::isatty(0) } == 1;
    let current: Arc<Mutex<Option<UnixStream>>> = Arc::default();
    if let Err(e) = forward_signals(current.clone(), reads_terminal) {
        return failed(&e);
    }
    // A detached run's command reads nothing: no client stays to give it input.
    let stdin = match command_stdin(request.interactive && !request.detach) {
        Ok(stdin) => stdin,
        Err(e) => return failed(&e),
    };
    // A daemon from another build answers RESTART once it has stepped aside.
    for _ in 0..2 {
        let conn = match connect(home, daemon, &mut started) {
            Ok(conn) => conn,
            Err(e) => return failed(&e),
        };
        let [out, err] = [1, 2].map(|fd| {
            // SAFETY: this process's standard descriptors stay open while it runs.
            unsafe { BorrowedFd::borrow_raw(fd) }
        });
        let stdio = [stdin.as_fd(), out, err];
        if let Err(e) = shards_ipc::send(&conn, kind::START, &request.encode(), &stdio) {
            return failed(&format!("asking the daemon: {e}"));
        }
        match conn.try_clone() {
            Ok(signals) => *current.lock().unwrap_or_else(PoisonError::into_inner) = Some(signals),
            Err(e) => return failed(&format!("the daemon's connection: {e}")),
        }
        loop {
            match shards_ipc::recv(&conn) {
                Ok(Some(m)) if m.kind == kind::EXIT => {
                    let Some((&status, timing)) = m.payload.split_first() else {
                        return failed("the command's microVM sent no status");
                    };
                    if !timing.is_empty() {
                        let _ = writeln!(io::stderr(), "shards-timing {}", String::from_utf8_lossy(timing));
                    }
                    return ExitCode::from(status);
                }
                Ok(Some(m)) if m.kind == kind::RESTART => break,
                // A detached run's answer: its container's ID, or why it did not start.
                Ok(Some(m)) if m.kind == kind::OUT => {
                    let _ = io::stdout().write_all(&m.payload);
                }
                Ok(Some(m)) if m.kind == kind::ERR => {
                    let _ = io::stderr().write_all(&m.payload);
                }
                Ok(Some(m)) if m.kind == kind::END => {
                    return ExitCode::from(m.payload.first().copied().unwrap_or(NOT_RUN));
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => {
                    return failed(&format!(
                        "the command's microVM stopped before the command ended; see {}",
                        log(home).display()
                    ));
                }
            }
        }
    }
    failed("the daemon kept asking for a restart")
}

/// Runs a container command (`ps`, `wait`, `rm`, ...) in the daemon of `home`, whose
/// binary is `daemon`, printing what the daemon answers as it comes, and exits with the
/// command's status.
pub fn container(home: &Path, daemon: &Path, command: &Command) -> ExitCode {
    let mut started = match enter(home, daemon) {
        Ok(started) => started,
        Err(e) => return failed(&e),
    };
    // A daemon from another build answers RESTART once it has stepped aside.
    for _ in 0..2 {
        let conn = match connect(home, daemon, &mut started) {
            Ok(conn) => conn,
            Err(e) => return failed(&e),
        };
        if let Err(e) = shards_ipc::send(&conn, kind::CONTAINER, &command.encode(), &[]) {
            return failed(&format!("asking the daemon: {e}"));
        }
        loop {
            match shards_ipc::recv(&conn) {
                Ok(Some(m)) if m.kind == kind::OUT => {
                    let _ = io::stdout().write_all(&m.payload);
                }
                Ok(Some(m)) if m.kind == kind::ERR => {
                    let _ = io::stderr().write_all(&m.payload);
                }
                Ok(Some(m)) if m.kind == kind::END => {
                    return ExitCode::from(m.payload.first().copied().unwrap_or(1));
                }
                Ok(Some(m)) if m.kind == kind::RESTART => break,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    return failed(&format!(
                        "the daemon hung up before it answered; see {}",
                        log(home).display()
                    ));
                }
            }
        }
    }
    failed("the daemon kept asking for a restart")
}

/// How long `stop` waits for the daemon to hand over the runs in hand and exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// Asks the daemon of `home` to exit once the runs in hand are handed over, and waits
/// until it has: the daemon holds this connection open until it exits.
pub fn stop(home: &Path) -> ExitCode {
    match std::env::set_current_dir(home) {
        Ok(()) => {}
        // No home: no daemon to stop.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ExitCode::SUCCESS,
        Err(e) => return failed(&format!("{}: {e}", home.display())),
    }
    match UnixStream::connect(SOCKET) {
        Ok(conn) => {
            if let Err(e) = shards_ipc::send(&conn, kind::STOP, &[], &[]) {
                return failed(&format!("asking the daemon to stop: {e}"));
            }
            if let Err(e) = conn.set_read_timeout(Some(STOP_TIMEOUT)) {
                return failed(&format!("the daemon's connection: {e}"));
            }
            match shards_ipc::recv(&conn) {
                Ok(None) => ExitCode::SUCCESS,
                Err(e) if !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    ExitCode::SUCCESS
                }
                Ok(Some(_)) | Err(_) => failed(&format!("the daemon did not exit within {STOP_TIMEOUT:?}")),
            }
        }
        // No daemon: nothing to stop.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => failed(&format!("{}: {e}", home.join(SOCKET).display())),
    }
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "shards: {message}");
    ExitCode::from(NOT_RUN)
}

/// Makes `home` this process's working directory. A home that does not exist yet is the
/// daemon's to make: then this starts the daemon and waits for it. Returns whether it
/// started one.
fn enter(home: &Path, daemon: &Path) -> Result<bool, String> {
    match std::env::set_current_dir(home) {
        Ok(()) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", home.display())),
    }
    start(daemon)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match std::env::set_current_dir(home) {
            Ok(()) => return Ok(true),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!("the daemon did not make {} ({e})", home.display()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// The daemon's connection, starting the daemon if none listens and `started` says this
/// process has not started one yet.
fn connect(home: &Path, daemon: &Path, started: &mut bool) -> Result<UnixStream, String> {
    match UnixStream::connect(SOCKET) {
        Ok(conn) => return Ok(conn),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(format!("{}: {e}", home.join(SOCKET).display())),
    }
    if !*started {
        start(daemon)?;
        *started = true;
    }
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match UnixStream::connect(SOCKET) {
            Ok(conn) => return Ok(conn),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "the daemon did not start ({e}); see {}",
                    log(home).display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Starts `daemon daemon --detached` in a session of its own; the daemon creates its home
/// and writes its messages to the log there. Of daemons started at once, one takes the
/// home's lock and the rest exit.
fn start(daemon: &Path) -> Result<(), String> {
    use std::os::fd::AsFd;
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    shards_ipc::spawn(
        daemon,
        &["daemon".as_ref(), "--detached".as_ref()],
        &[(null.as_fd(), 0), (null.as_fd(), 1), (null.as_fd(), 2)],
        true,
    )
    .map(drop)
    .map_err(|e| format!("starting the daemon {}: {e}", daemon.display()))
}

/// The command's stdin: /dev/null, or with `interactive` a pipe a thread fills from this
/// process's stdin, closing it when that ends or this process exits.
fn command_stdin(interactive: bool) -> Result<OwnedFd, String> {
    if !interactive {
        return File::open("/dev/null")
            .map(OwnedFd::from)
            .map_err(|e| format!("/dev/null: {e}"));
    }
    let (reader, mut writer) = io::pipe().map_err(|e| format!("a pipe for stdin: {e}"))?;
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let mut stdin = io::stdin().lock();
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        if writer.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        })
        .map_err(|e| format!("stdin thread: {e}"))?;
    Ok(OwnedFd::from(reader))
}

/// Sends the signals `docker run` forwards to the command, over the current connection,
/// even those this process was started ignoring, as the Docker CLI does
/// (shards_ipc::take_forwarded). They are blocked in the calling thread, which every
/// thread started later inherits, so only the forwarder's `sigwait` receives them. One that
/// would end the client, arriving with no connection to send it on, ends the client as it
/// would have, unless it was ignored. With `reads_terminal`, the terminal's job control
/// applies to the client (shards_ipc::forwarded).
fn forward_signals(current: Arc<Mutex<Option<UnixStream>>>, reads_terminal: bool) -> Result<(), String> {
    let (set, ignored) =
        shards_ipc::take_forwarded(reads_terminal).map_err(|e| format!("taking signals: {e}"))?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let mut sig = 0;
                // SAFETY: sigwait(3) on a valid set.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    return;
                }
                let Some(&(_, linux)) = shards_ipc::FORWARDED.iter().find(|(s, _)| *s == sig) else {
                    continue;
                };
                let sent = current
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .is_some_and(|conn| {
                        shards_ipc::send(conn, kind::SIGNAL, &linux.to_be_bytes(), &[]).is_ok()
                    });
                let ends = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&sig);
                if !sent && ends && !ignored.contains(&sig) {
                    // SAFETY: the default action of a terminating signal, on this process.
                    unsafe {
                        libc::signal(sig, libc::SIG_DFL);
                        let mut only: libc::sigset_t = std::mem::zeroed();
                        libc::sigemptyset(&mut only);
                        libc::sigaddset(&mut only, sig);
                        libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
                        libc::raise(sig);
                    }
                }
            }
        })
        .map_err(|e| format!("signal thread: {e}"))?;
    Ok(())
}
