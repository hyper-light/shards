//! The client side of `shards run` (docs/design/architecture.md D26). It asks the daemon
//! for the run, starting the daemon if there is none, and passes it this process's stdout
//! and stderr, which the command then writes to itself. The command's stdin is /dev/null,
//! or with `-i` a pipe the client fills from its own stdin: then the command's stdin ends
//! when the client does, as `docker run -i`'s does when its client goes (StdinOnce), and
//! only the client reads its terminal. The client forwards the signals it gets, as
//! `docker run` does, and exits with the command's status.
//!
//! With `-t`, the command's stdio is a pty in the guest. With `-it` the client puts its
//! terminal in raw mode for the run, as the Docker CLI does, and detaches, exiting 0, when
//! the detach keys come; a change in its stdout's size resizes the pty
//! (docs/research/tty-and-interactive-runs.md).
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

use shards_cmdline::term::{EscapeProxy, Read as Typed};
use shards_ipc::{Command, Run, SOCKET, kind, log};

use crate::{NOT_RUN, terminal};

/// How long a started daemon may take to listen: longer than the one it replaces may take
/// to end its runs and exit (daemon.rs, TAKEOVER).
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `request` through the daemon of `home`, whose binary is `daemon`, detaching on
/// `detach_keys` under `-it`. Every path the request holds is absolute by now
/// (request.rs), since this process makes the home its working directory, where the
/// daemon's socket is (`shards_ipc::SOCKET`). The terminal is back as it was when this
/// returns.
pub fn run(home: &Path, daemon: &Path, request: &Run, detach_keys: &[u8]) -> ExitCode {
    let code = serve(home, daemon, request, detach_keys);
    terminal::restore();
    code
}

fn serve(home: &Path, daemon: &Path, request: &Run, detach_keys: &[u8]) -> ExitCode {
    // Before any thread starts, none of which may use a relative path meanwhile.
    let mut started = match enter(home, daemon) {
        Ok(started) => started,
        Err(e) => return failed(&e),
    };
    // SAFETY: isatty(3) on this process's stdin and stdout.
    let (in_terminal, out_terminal) = unsafe { (libc::isatty(0) == 1, libc::isatty(1) == 1) };
    let attached = request.interactive && !request.detach;
    let reads_terminal = request.interactive && in_terminal;
    // A terminal's size follows the client's stdout's, when it is one (docker/cli
    // cli/command/container/run.go, MonitorTtySize).
    let resizes = request.tty.is_some() && !request.detach && out_terminal;
    let current: Arc<Mutex<Option<UnixStream>>> = Arc::default();
    if let Err(e) = forward_signals(current.clone(), reads_terminal, resizes) {
        return failed(&e);
    }
    // Only an attached stdin, and only on a terminal, goes raw, unless NORAW is set; the
    // detach keys are looked for only then (docker/cli hijack.go, streams/in.go).
    let raw = request.tty.is_some()
        && attached
        && in_terminal
        && std::env::var_os("NORAW").is_none_or(|v| v.is_empty());
    let proxy = (request.tty.is_some() && attached).then(|| EscapeProxy::new(detach_keys));
    // A detached run's command reads nothing: no client stays to give it input.
    let stdin = match command_stdin(attached, proxy) {
        Ok(stdin) => stdin,
        Err(e) => return failed(&e),
    };
    if raw && let Err(e) = terminal::make_raw() {
        return failed(&format!("the terminal's raw mode: {e}"));
    }
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
        // The size as the command starts, in case it changed since the request.
        if resizes {
            resize(&conn);
        }
        loop {
            match shards_ipc::recv(&conn) {
                Ok(Some(m)) if m.kind == kind::EXIT => {
                    let Some((&status, timing)) = m.payload.split_first() else {
                        return failed("the command's microVM sent no status");
                    };
                    if !timing.is_empty() {
                        let line = with_client_rss(&String::from_utf8_lossy(timing));
                        let _ = writeln!(io::stderr(), "shards-timing {line}");
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

/// The VM's timing line, a JSON object, with this client's own peak RSS in KiB as its
/// first field, `client_rss_kib`, for benchmarks.
fn with_client_rss(timing: &str) -> String {
    let kib = shards_ipc::peak_rss_kib();
    match timing.strip_prefix('{') {
        Some(rest) if rest.trim_start().starts_with('}') => format!("{{\"client_rss_kib\":{kib}{rest}"),
        Some(rest) => format!("{{\"client_rss_kib\":{kib},{rest}"),
        None => timing.to_string(),
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

/// Runs `daemon daemon --detached`, which starts the daemon in the background and exits;
/// the daemon creates its home and writes its messages to the log there. Of daemons
/// started at once, one takes the home's lock and the rest exit.
fn start(daemon: &Path) -> Result<(), String> {
    use std::os::fd::AsFd;
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let starting = |e: io::Error| format!("starting the daemon {}: {e}", daemon.display());
    let starter = shards_ipc::spawn(
        daemon,
        &["daemon".as_ref(), "--detached".as_ref()],
        &[(null.as_fd(), 0), (null.as_fd(), 1), (null.as_fd(), 2)],
        true,
    )
    .map_err(starting)?;
    match starter.wait().map_err(starting)? {
        0 => Ok(()),
        status => Err(format!(
            "starting the daemon {}: exit status {status}",
            daemon.display()
        )),
    }
}

/// The command's stdin: /dev/null, or with `interactive` a pipe a thread fills from this
/// process's stdin, closing it when that ends or this process exits. With a `proxy`, the
/// detach keys end the client instead, with status 0 and the terminal restored, and the
/// command runs on, as a container outlives the `docker run` that detached from it.
fn command_stdin(interactive: bool, mut proxy: Option<EscapeProxy>) -> Result<OwnedFd, String> {
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
                let read = match stdin.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => buf.get(..n).unwrap_or_default(),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                };
                let typed = match proxy.as_mut() {
                    Some(proxy) => proxy.read(read),
                    None => Typed::Input(read.to_vec()),
                };
                let (input, detached) = match typed {
                    Typed::Input(input) => (input, false),
                    Typed::Detach(before) => (before, true),
                };
                if writer.write_all(&input).is_err() {
                    return;
                }
                if detached {
                    terminal::restore();
                    std::process::exit(0);
                }
            }
        })
        .map_err(|e| format!("stdin thread: {e}"))?;
    Ok(OwnedFd::from(reader))
}

/// Sends the size of this process's stdout as the command's terminal's.
fn resize(conn: &UnixStream) {
    let (rows, cols) = terminal::size(1);
    if rows != 0 && cols != 0 {
        let [a, b] = rows.to_be_bytes();
        let [c, d] = cols.to_be_bytes();
        let _ = shards_ipc::send(conn, kind::RESIZE, &[a, b, c, d], &[]);
    }
}

/// Sends the signals `docker run` forwards to the command, over the current connection,
/// even those this process was started ignoring, as the Docker CLI does
/// (shards_ipc::take_forwarded). They are blocked in the calling thread, which every
/// thread started later inherits, so only the forwarder's `sigwait` receives them. One that
/// would end the client, arriving with no connection to send it on, ends the client as it
/// would have, unless it was ignored, with the terminal restored first. With
/// `reads_terminal`, the terminal's job control applies to the client
/// (shards_ipc::forwarded). With `resizes`, SIGWINCH first resizes the command's terminal,
/// then goes to the command too, as both reach a Docker container's (docker/cli tty.go,
/// signals.go).
fn forward_signals(
    current: Arc<Mutex<Option<UnixStream>>>,
    reads_terminal: bool,
    resizes: bool,
) -> Result<(), String> {
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
                        if resizes && sig == libc::SIGWINCH {
                            resize(conn);
                        }
                        shards_ipc::send(conn, kind::SIGNAL, &linux.to_be_bytes(), &[]).is_ok()
                    });
                let ends = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&sig);
                if !sent && ends && !ignored.contains(&sig) {
                    terminal::restore();
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
