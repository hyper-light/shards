//! The client side of `shards run` (docs/design/architecture.md D26). It asks the daemon
//! for the run, starting the daemon if there is none, and passes it this process's stdout
//! and stderr, which the command then writes to itself. The command's stdin is /dev/null,
//! or with `-i` a pipe the client fills from its own stdin: then the command's stdin ends
//! when the client does, as `docker run -i`'s does when its client goes (StdinOnce), and
//! only the client reads its terminal. The client forwards the signals it gets, as
//! `docker run` does, once its container is made; before, SIGINT and SIGTERM end it, and
//! with it the run, as they end `docker run` as it pulls. It exits with the command's
//! status.
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
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_cmdline::term::{EscapeProxy, Read as Typed};
use shards_ipc::{Command, Run, SOCKET, kind, log};

use crate::cli::{NOT_RUN, terminal};

/// How long a started daemon may take to listen, once the one it replaces has ended its
/// runs (shards_ipc::exiting), which takes as long as their stop timeouts make it.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `request` through the daemon of `home`, whose binary is `daemon`, detaching on
/// `detach_keys` under `-it`. Every path the request holds is absolute by now
/// (request.rs), since this process makes the home its working directory, where the
/// daemon's socket is (`shards_ipc::SOCKET`). The terminal is back as it was when this
/// returns.
pub fn run(home: &Path, daemon: &Path, request: &Run, detach_keys: &[u8]) -> ExitCode {
    let asked = Attached {
        kind: kind::START,
        payload: request.encode(),
        interactive: request.interactive,
        detach: request.detach,
        tty: request.tty.is_some(),
        proxies_signals: true,
        card: request
            .detach
            .then(|| (request.image.clone(), request.name.clone())),
    };
    let code = serve(home, daemon, &asked, detach_keys);
    terminal::restore();
    code
}

/// Runs `request` in a running container through the daemon of `home`, as `docker exec`
/// does: the command's stdio is this process's, and its status this process's, but
/// signals are not passed on, as `docker exec` passes none.
pub fn exec(home: &Path, daemon: &Path, request: &shards_ipc::Exec, detach_keys: &[u8]) -> ExitCode {
    let asked = Attached {
        kind: kind::EXEC,
        payload: request.encode(),
        interactive: request.interactive,
        detach: request.detach,
        tty: request.tty.is_some(),
        proxies_signals: false,
        card: None,
    };
    let code = serve(home, daemon, &asked, detach_keys);
    terminal::restore();
    code
}

/// What a client attached to a command asks the daemon, and how it attaches.
struct Attached {
    kind: u8,
    payload: Vec<u8>,
    interactive: bool,
    detach: bool,
    tty: bool,
    /// The signals `docker run` passes on go to the command (not `docker exec`'s).
    proxies_signals: bool,
    /// A detached run's image and name: on a colour terminal its ID is shown in a card.
    card: Option<(String, Option<String>)>,
}

fn serve(home: &Path, daemon: &Path, request: &Attached, detach_keys: &[u8]) -> ExitCode {
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
    let resizes = request.tty && !request.detach && out_terminal;
    // The connection the run is on, where signals go: one run a process, so the forwarder,
    // which lives as long as the process, shares it as a `static`.
    static CURRENT: Mutex<Option<UnixStream>> = Mutex::new(None);
    let current = &CURRENT;
    // Signals are taken now, before any thread starts, as each inherits what this one
    // blocks; the threads that forward them and fill the command's stdin start once the
    // request is on its way, while the daemon and the VM work on it (review 8.12).
    let signals = match take_signals(reads_terminal, resizes, request.proxies_signals) {
        Ok(signals) => signals,
        Err(e) => return failed(&e),
    };
    // Only an attached stdin, and only on a terminal, goes raw, unless NORAW is set; the
    // detach keys are looked for only then (docker/cli hijack.go, streams/in.go).
    let raw =
        request.tty && attached && in_terminal && std::env::var_os("NORAW").is_none_or(|v| v.is_empty());
    let proxy = (request.tty && attached).then(|| EscapeProxy::new(detach_keys));
    // A detached run's command reads nothing: no client stays to give it input.
    let (stdin, filler) = match command_stdin(attached) {
        Ok(stdin) => stdin,
        Err(e) => return failed(&e),
    };
    if raw && let Err(e) = terminal::make_raw() {
        return failed(&format!("the terminal's raw mode: {e}"));
    }
    let mut threads = Some((signals, filler, proxy));
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
        if let Err(e) = shards_ipc::send(&conn, request.kind, &request.payload, &stdio) {
            return failed(&format!("asking the daemon: {e}"));
        }
        if let Some((signals, filler, proxy)) = threads.take() {
            if let Err(e) = forward(signals, current) {
                return failed(&e);
            }
            if let Some(filler) = filler
                && let Err(e) = fill_stdin(filler, proxy)
            {
                return failed(&e);
            }
        }
        // Signals go to the command, and its terminal follows ours, once it is there: a
        // run's once the daemon has made its container, an exec's at once.
        let attach = || -> Result<(), String> {
            let signals = conn
                .try_clone()
                .map_err(|e| format!("the daemon's connection: {e}"))?;
            *current.lock().unwrap_or_else(PoisonError::into_inner) = Some(signals);
            // The size as the command starts, in case it changed since the request.
            if resizes {
                resize(&conn);
            }
            Ok(())
        };
        if request.kind != kind::START
            && let Err(e) = attach()
        {
            return failed(&e);
        }
        loop {
            match shards_ipc::recv(&conn) {
                Ok(Some(m)) if m.kind == kind::CREATED => {
                    if let Err(e) = attach() {
                        return failed(&e);
                    }
                }
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
                    if let (Some((image, name)), Some(p)) = (&request.card, crate::cli::look::styled()) {
                        let id = String::from_utf8_lossy(&m.payload).trim().to_string();
                        crate::cli::look::started(&p, &id, image, name.as_deref(), &mut io::stdout().lock());
                    } else {
                        let _ = io::stdout().write_all(&m.payload);
                    }
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
pub fn container(home: &Path, daemon: &Path, command: &Command, fds: &[std::os::fd::BorrowedFd<'_>]) -> u8 {
    let failed = |message: &str| {
        let _ = writeln!(io::stderr(), "shards: {message}");
        NOT_RUN
    };
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
        if let Err(e) = shards_ipc::send(&conn, kind::CONTAINER, &command.encode(), fds) {
            return failed(&format!("asking the daemon: {e}"));
        }
        let answered = std::thread::scope(|scope| answer(scope, &conn, home, command.east_asian));
        if let Some(status) = answered {
            return status;
        }
    }
    failed("the daemon kept asking for a restart")
}

/// The daemon's answer on `conn`: its output, as it comes, and its status, or `None`
/// when it asks for a restart. Steps of a pull, which the daemon sends a terminal, are
/// shown by a display of their own (show.rs), on a thread of this `scope`; the daemon's
/// text then goes through it, below its frame.
fn answer<'s>(
    scope: &'s std::thread::Scope<'s, '_>,
    conn: &UnixStream,
    home: &Path,
    east_asian: bool,
) -> Option<u8> {
    use crate::cli::show::{self, Shown};
    let mut display: Option<std::sync::mpsc::Sender<Shown>> = None;
    let mut refused = false;
    // On a colour terminal the daemon's errors are gathered, and shown in a panel at the
    // end: under the page drawn, if one was, or under the head.
    let error_paint = crate::cli::look::styled_err();
    let mut errors: Vec<u8> = Vec::new();
    let mut paged = false;
    let pass = |display: &Option<std::sync::mpsc::Sender<Shown>>, shown: Shown| match (display, shown) {
        (Some(tx), shown) => {
            let _ = tx.send(shown);
        }
        (None, Shown::Out(bytes)) => {
            let _ = io::stdout().write_all(&bytes);
        }
        (None, Shown::Err(bytes)) => {
            let _ = io::stderr().write_all(&bytes);
        }
        (None, Shown::Progress(_)) => {}
    };
    loop {
        match shards_ipc::recv(conn) {
            Ok(Some(m)) if m.kind == kind::PROGRESS => {
                let Some(event) = shards_ipc::Progress::decode(&m.payload) else {
                    continue;
                };
                if display.is_none() && !refused {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let env = |k: &str| std::env::var(k).ok();
                    let mut paint = shards_tui::tokens::Paint::new(shards_tui::tokens_truecolor(&env));
                    if let Some(page) = crate::cli::terminal::background() {
                        paint.page = page;
                    }
                    let pull = show::Pull::new(paint, east_asian, shards_tui::motion::reduced(env));
                    let started = std::thread::Builder::new()
                        .name("shards-show".into())
                        .spawn_scoped(scope, move || {
                            show::run(rx, pull, || {
                                let (rows, cols) = crate::cli::terminal::size(1);
                                (usize::from(rows), usize::from(cols))
                            })
                        });
                    match started {
                        Ok(_) => display = Some(tx),
                        // No thread to show it on: the steps go unshown, the rest is printed.
                        Err(_) => refused = true,
                    }
                }
                pass(&display, Shown::Progress(event));
            }
            Ok(Some(m)) if m.kind == kind::SHEET => {
                if let Some(sheet) = shards_ipc::Sheet::decode(&m.payload) {
                    crate::cli::screens::show(&sheet);
                    paged = true;
                }
            }
            Ok(Some(m)) if m.kind == kind::OUT => pass(&display, Shown::Out(m.payload)),
            Ok(Some(m)) if m.kind == kind::ERR && error_paint.is_some() => {
                errors.extend_from_slice(&m.payload);
            }
            Ok(Some(m)) if m.kind == kind::ERR => pass(&display, Shown::Err(m.payload)),
            Ok(Some(m)) if m.kind == kind::END => {
                // The display ends with the channel, and the scope waits for it.
                paged |= display.is_some();
                drop(display);
                if let (Some(p), false) = (&error_paint, errors.is_empty()) {
                    let text = String::from_utf8_lossy(&errors);
                    let said: Vec<&str> = text
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(|l| l.strip_prefix("Error response from daemon: ").unwrap_or(l))
                        .collect();
                    crate::cli::look::panel(p, &said.join("\n"), !paged);
                }
                return Some(m.payload.first().copied().unwrap_or(1));
            }
            Ok(Some(m)) if m.kind == kind::RESTART => return None,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                drop(display);
                let _ = writeln!(
                    io::stderr(),
                    "shards: the daemon hung up before it answered; see {}",
                    log(home).display()
                );
                return Some(NOT_RUN);
            }
        }
    }
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
    start(daemon, home)?;
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
        start(daemon, home)?;
        *started = true;
    }
    let mut deadline = Instant::now() + START_TIMEOUT;
    loop {
        if shards_ipc::exiting(home) {
            deadline = Instant::now() + START_TIMEOUT;
        }
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
/// started at once, one takes the home's lock and the rest exit. What the starter says
/// when it fails, a setting the daemon cannot keep, is what this says.
fn start(daemon: &Path, home: &Path) -> Result<(), String> {
    use std::os::fd::AsFd;
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let starting = |e: io::Error| format!("starting the daemon {}: {e}", daemon.display());
    let (mut said, speaks) = io::pipe().map_err(starting)?;
    // The home this client resolved, before it made the home its working directory
    // (audit A17).
    let starter = shards_ipc::spawn_with(
        daemon,
        &["daemon".as_ref(), "--detached".as_ref()],
        &[(null.as_fd(), 0), (null.as_fd(), 1), (speaks.as_fd(), 2)],
        true,
        &[(shards_ipc::HOME, home.as_os_str())],
    )
    .map_err(starting)?;
    drop(speaks);
    // What it says, read to its end before it is waited for: a starter whose words filled
    // the pipe would otherwise wait on this as this waits on it (review 8.14). The first
    // 64 KiB are kept.
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match said.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = (64 * 1024usize).saturating_sub(kept.len());
                kept.extend_from_slice(chunk.get(..n.min(room)).unwrap_or_default());
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&kept).into_owned();
    let status = starter.wait().map_err(starting)?;
    match status {
        0 => Ok(()),
        status if text.trim().is_empty() => Err(format!(
            "starting the daemon {}: exit status {status}",
            daemon.display()
        )),
        _ => Err(text.trim().to_string()),
    }
}

/// The command's stdin: /dev/null, or with `interactive` a pipe [`fill_stdin`] fills from
/// this process's stdin, whose writing end is returned for it.
fn command_stdin(interactive: bool) -> Result<(OwnedFd, Option<io::PipeWriter>), String> {
    if !interactive {
        return File::open("/dev/null")
            .map(|f| (OwnedFd::from(f), None))
            .map_err(|e| format!("/dev/null: {e}"));
    }
    let (reader, writer) = io::pipe().map_err(|e| format!("a pipe for stdin: {e}"))?;
    Ok((OwnedFd::from(reader), Some(writer)))
}

/// Fills `writer`, the command's stdin, from this process's stdin on a thread of its own,
/// closing it when that ends or this process exits. With a `proxy`, the detach keys end
/// the client instead, with status 0 and the terminal restored, and the command runs on,
/// as a container outlives the `docker run` that detached from it.
fn fill_stdin(mut writer: io::PipeWriter, mut proxy: Option<EscapeProxy>) -> Result<(), String> {
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
                // Without a proxy, what was read goes on as it is, uncopied (audit D08).
                let Some(proxy) = proxy.as_mut() else {
                    if writer.write_all(read).is_err() {
                        return;
                    }
                    continue;
                };
                let (input, detached) = match proxy.read(read) {
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
        .map(drop)
        .map_err(|e| format!("stdin thread: {e}"))
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

/// Takes the signals `docker run` forwards to the command, for [`forward`] to send over the
/// current connection, even those this process was started ignoring, as the Docker CLI
/// does (shards_ipc::take_forwarded). They are blocked in the calling thread, which every
/// thread started later inherits, so only the forwarder's `sigwait` receives them. One that
/// would end the client, arriving with no connection to send it on, ends the client,
/// unless it was ignored, with the terminal restored first: SIGINT and SIGTERM as they
/// end the Docker CLI before its container is made, with 128 and their number and
/// nothing said (docker/cli cmd/docker/docker.go, notifyContext); SIGHUP and SIGQUIT as
/// they would have. With `reads_terminal`, the terminal's job control applies to the
/// client (shards_ipc::forwarded). With `resizes`, SIGWINCH first resizes the command's
/// terminal, then goes to the command too, as both reach a Docker container's (docker/cli
/// tty.go, signals.go). Without `proxies`, only SIGWINCH is taken, to resize the command's
/// terminal with `resizes`, and the rest act on the client as they would.
fn take_signals(reads_terminal: bool, resizes: bool, proxies: bool) -> Result<Option<Signals>, String> {
    if !proxies {
        if !resizes {
            return Ok(None);
        }
        // SAFETY: plain sigset operations on a local set, then blocking it in this thread,
        // which threads started later inherit.
        let set = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGWINCH);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            set
        };
        return Ok(Some(Signals {
            set,
            ignored: Vec::new(),
            resizes,
            proxies,
        }));
    }
    let (mut set, ignored) =
        shards_ipc::take_forwarded(reads_terminal).map_err(|e| format!("taking signals: {e}"))?;
    // And the rest Docker forwards: Linux's real-time signals, and the signals a fault
    // raises, those another process sent (review 8.17).
    shards_ipc::take_rest(&mut set).map_err(|e| format!("taking signals: {e}"))?;
    Ok(Some(Signals {
        set,
        ignored,
        resizes,
        proxies,
    }))
}

/// The signals [`take_signals`] took, for [`forward`]: the set blocked, those that were
/// ignored, and what is done with them.
struct Signals {
    set: libc::sigset_t,
    ignored: Vec<libc::c_int>,
    resizes: bool,
    proxies: bool,
}

/// Starts the thread that receives `signals`, as [`take_signals`] says, on the run's
/// connection in `current` once there is one.
fn forward(signals: Option<Signals>, current: &'static Mutex<Option<UnixStream>>) -> Result<(), String> {
    let Some(Signals {
        set,
        ignored,
        resizes,
        proxies,
    }) = signals
    else {
        return Ok(());
    };
    if !proxies {
        std::thread::Builder::new()
            .name("resizes".into())
            .spawn(move || {
                let mut sig = 0;
                // SAFETY: sigwait(3) on a valid set.
                while unsafe { libc::sigwait(&set, &mut sig) } == 0 {
                    if let Some(conn) = current.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                        resize(conn);
                    }
                }
            })
            .map_err(|e| format!("resize thread: {e}"))?;
        return Ok(());
    }
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            shards_ipc::wait_here();
            loop {
                let mut sig = 0;
                // SAFETY: sigwait(3) on a valid set.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    return;
                }
                if sig == shards_ipc::CARRIER {
                    if let Some(conn) = current.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                        for linux in shards_ipc::sent_faults() {
                            let _ = shards_ipc::send(conn, kind::SIGNAL, &linux.to_be_bytes(), &[]);
                        }
                    }
                    continue;
                }
                let Some(linux) = shards_ipc::linux_signal(sig) else {
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
                    if sig == libc::SIGINT || sig == libc::SIGTERM {
                        std::process::exit(128 + sig);
                    }
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
