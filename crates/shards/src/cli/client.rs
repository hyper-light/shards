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
use std::io::{self, IsTerminal as _, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_cmdline::term::{EscapeProxy, Read as Typed};
#[cfg(unix)]
use shards_ipc::SOCKET;
#[cfg(unix)]
use shards_ipc::fd::OwnedFd;
use shards_ipc::fd::{AsFd, BorrowedFd};
use shards_ipc::{Command, Run, kind, log};

use crate::cli::{NOT_RUN, terminal};

/// A signal as this client takes it: the host's own number on Unix; on Windows, Linux's,
/// as which the console's events are taken ([`console_signal`]).
#[cfg(unix)]
type Sig = libc::c_int;
#[cfg(windows)]
type Sig = i32;

#[cfg(unix)]
const SIGINT: Sig = libc::SIGINT;
#[cfg(windows)]
const SIGINT: Sig = shards_ipc::signal::SIGINT;

/// How many times a client asks before it gives up on daemons asking it to ask again
/// (RESTART): a daemon of another build answers so the request that makes it step aside,
/// and then each it took meanwhile through a slot it had offered of the kind the client
/// asks for (`slots::OFFERED`); one of a build from before slots, once, on its socket.
const RESTARTS: usize = shards_ipc::slots::OFFERED + 2;

/// How long a started daemon may take to listen, once the one it replaces has ended its
/// runs (shards_ipc::exiting), which takes as long as their stop timeouts make it.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `request` through the daemon of `home`, whose binary is `daemon`, detaching on
/// `detach_keys` under `-it`. Every path the request holds is absolute by now
/// (request.rs), since this process makes the home its working directory, where the
/// daemon's socket is (`shards_ipc::SOCKET`). The terminal is back as it was when this
/// returns. Signals go on to the command with `sig_proxy` (`--sig-proxy`), and the
/// container's ID into `cid` once it is made (`--cidfile`).
pub fn run(
    home: &Path,
    daemon: &Path,
    request: &Run,
    detach_keys: &[u8],
    sig_proxy: bool,
    cid: &mut Option<crate::cli::request::CidFile>,
) -> ExitCode {
    let given = |stream: &str| request.attach.is_empty() || request.attach.iter().any(|a| a == stream);
    let asked = Attached {
        kind: kind::START,
        payload: request.encode(),
        interactive: request.interactive,
        detach: request.detach,
        tty: request.tty.is_some(),
        streams: (given("stdout"), given("stderr")),
        proxies_signals: sig_proxy,
        card: request
            .detach
            .then(|| (request.image.clone(), request.name.clone())),
        detach_said: None,
    };
    let code = serve(home, daemon, &asked, detach_keys, cid);
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
        streams: (true, true),
        proxies_signals: false,
        card: None,
        detach_said: None,
    };
    let code = serve(home, daemon, &asked, detach_keys, &mut None);
    terminal::restore();
    code
}

/// Attaches to a running container's command (`shards attach`), whose terminal `tty`
/// says, its signals passed on with `proxies_signals`, as `docker attach` attaches; a
/// detach is said as docker/cli says it, and exits 1 (attach.go: the streamer's
/// EscapeError).
pub fn attach(
    home: &Path,
    daemon: &Path,
    request: &shards_ipc::Attach,
    tty: bool,
    proxies_signals: bool,
    detach_keys: &[u8],
) -> ExitCode {
    let asked = Attached {
        kind: kind::ATTACH,
        payload: request.encode(),
        interactive: request.stdin,
        detach: false,
        tty,
        streams: (true, true),
        proxies_signals,
        card: None,
        detach_said: Some(("read escape sequence", 1)),
    };
    let code = serve(home, daemon, &asked, detach_keys, &mut None);
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
    /// Whether its stdout and its stderr are this process's: `run -a` may leave either out,
    /// its output then kept in its log alone.
    streams: (bool, bool),
    /// The signals `docker run` passes on go to the command (not `docker exec`'s).
    proxies_signals: bool,
    /// A detached run's image and name: on a colour terminal its ID is shown in a card.
    card: Option<(String, Option<String>)>,
    /// What the client says as it leaves by its detach keys, and its status; none for 0.
    detach_said: Option<(&'static str, u8)>,
}

fn serve(
    home: &Path,
    daemon: &Path,
    request: &Attached,
    detach_keys: &[u8],
    cid: &mut Option<crate::cli::request::CidFile>,
) -> ExitCode {
    use shards_ipc::slots;
    // Before any thread starts, none of which may use a relative path meanwhile.
    let mut started = match enter(home, daemon) {
        Ok(started) => started,
        Err(e) => return failed(&e),
    };
    let (in_terminal, out_terminal) = (io::stdin().is_terminal(), io::stdout().is_terminal());
    let attached = request.interactive && !request.detach;
    let reads_terminal = request.interactive && in_terminal;
    // A terminal's size follows the client's stdout's, when it is one (docker/cli
    // cli/command/container/run.go, MonitorTtySize).
    let resizes = request.tty && !request.detach && out_terminal;
    // Where the run's signals go: its slot's `say`, or a socket's. One run a process, so the
    // forwarder, which lives as long as the process, shares it as a `static`.
    static CURRENT: Mutex<Current> = Mutex::new(Current::new());
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
    if raw && let Err(e) = terminal::make_raw() {
        return failed(&format!("the terminal's raw mode: {e}"));
    }
    let mut signals = Some(signals);
    // Their forwarder, once anything may be waited for: then SIGINT and SIGTERM end the
    // client as it waits for a daemon to start, as they end the Docker CLI before its
    // container is made; else once the request is on its way.
    let forwarding = |signals: &mut Option<Option<Signals>>| match signals.take() {
        Some(taken) => forward(taken, current),
        None => Ok(()),
    };
    // The command's stdin is filled once, from this client's; a detached run's command
    // reads nothing: no client stays to give it input.
    let mut filling = attached.then_some(proxy);
    // Which of the command's stdio is this client's: its stdin where attached, its stdout
    // and stderr where given. `run -a` may leave either out (`/dev/null`, its output kept
    // in its log alone). A detached run's are the daemon's alone, to say why it refused
    // the run: the daemon gives its VM none of them, so they end with the request.
    let ways = [
        (attached, slots::streams::IN, slots::fds::IN),
        (request.streams.0, slots::streams::OUT, slots::fds::OUT),
        (request.streams.1, slots::streams::ERR, slots::fds::ERR),
    ];
    let opened = ways.iter().filter(|w| w.0).fold(0, |all, w| all | w.1);
    let named = ways.map(|(given, _, fd)| if given { fd } else { slots::fds::NULL });
    // A daemon from another build answers RESTART once it has stepped aside.
    for _ in 0..RESTARTS {
        let asked = match ask_slot(
            home,
            daemon,
            &mut started,
            request.kind,
            &request.payload,
            opened,
            &named,
            &mut || forwarding(&mut signals),
        ) {
            Ok(asked) => asked,
            Err(e) => return failed(&e),
        };
        let ran = match asked {
            Asked::Slot(mut client) => {
                match client.sender() {
                    Ok(say) => hold(current, Says::Slot(say)),
                    Err(e) => return failed(&format!("its slot: {e}")),
                }
                if let Err(e) = forwarding(&mut signals) {
                    return failed(&e);
                }
                // Its stdin is filled once the daemon has said the request is its own to
                // serve (HEARD): what went to a daemon that then asked for a restart would
                // go with its slot.
                let mut input = client.input.take();
                let mut unfilled: Option<String> = None;
                std::thread::scope(|scope| {
                    // Its stdout and stderr, shown as they come: all of them before this
                    // client ends.
                    let mut shows = Vec::new();
                    for (from, which) in [(client.output.take(), 1), (client.errors.take(), 2)] {
                        let Some(from) = from else {
                            continue;
                        };
                        let to = match own_stdio(which) {
                            Ok(to) => to,
                            Err(e) => return Ran::Failed(format!("this client's stdio: {e}")),
                        };
                        match std::thread::Builder::new()
                            .name("shards-show".into())
                            .spawn_scoped(scope, move || show(&from, to))
                        {
                            Ok(shown) => shows.push(shown),
                            Err(e) => return Ran::Failed(format!("a stream's thread: {e}")),
                        }
                    }
                    let ran = ran_on(request, home, cid, current, resizes, &mut || {
                        loop {
                            match slot_message(&client)? {
                                Some(m) if m.kind == kind::HEARD => {
                                    if let (Some(input), Some(proxy)) = (input.take(), filling.take())
                                        && let Err(e) =
                                            fill_stdin(input, proxy, request.detach_said, current, resizes)
                                    {
                                        unfilled = Some(e);
                                        return Ok(None);
                                    }
                                }
                                m => return Ok(m),
                            }
                        }
                    });
                    for shown in shows {
                        let _ = shown.join();
                    }
                    if let Some(e) = unfilled.take() {
                        return Ran::Failed(e);
                    }
                    ran
                })
            }
            #[cfg(unix)]
            Asked::Socket(conn) => {
                let (stdin, filler) = match command_stdin(attached) {
                    Ok(stdin) => stdin,
                    Err(e) => return failed(&e),
                };
                // What a stream not attached is written to (`run -a`).
                let null = match request.streams {
                    (true, true) => None,
                    _ => match File::options().write(true).open("/dev/null") {
                        Ok(null) => Some(null),
                        Err(e) => return failed(&format!("/dev/null: {e}")),
                    },
                };
                let [out, err] =
                    [(1, request.streams.0), (2, request.streams.1)].map(|(fd, given)| match &null {
                        Some(null) if !given => null.as_fd(),
                        // SAFETY: this process's standard descriptors stay open while it runs.
                        _ => unsafe { BorrowedFd::borrow_raw(fd) },
                    });
                let stdio = [stdin.as_fd(), out, err];
                match shards_ipc::send(&conn, request.kind, &request.payload, &stdio) {
                    // Shut before it read the request, as a daemon of another build steps
                    // aside: the next one is asked.
                    Err(e) if unread(&e) => continue,
                    Err(e) => return failed(&format!("asking the daemon: {e}")),
                    Ok(()) => {}
                }
                match conn.try_clone() {
                    Ok(said) => hold(current, Says::Socket(said)),
                    Err(e) => return failed(&format!("the daemon's connection: {e}")),
                }
                if let Err(e) = forwarding(&mut signals) {
                    return failed(&e);
                }
                if let Some(filler) = filler
                    && let Some(proxy) = filling.take()
                    && let Err(e) = fill_stdin(
                        File::from(OwnedFd::from(filler)),
                        proxy,
                        request.detach_said,
                        current,
                        resizes,
                    )
                {
                    return failed(&e);
                }
                ran_on(request, home, cid, current, resizes, &mut || {
                    shards_ipc::recv(&conn)
                })
            }
        };
        match ran {
            Ran::Ended(status, timing) => {
                // Said once its output is all shown, as it was the last the command said.
                if let Some(timing) = timing {
                    let _ = writeln!(io::stderr(), "shards-timing {timing}");
                }
                return status;
            }
            // What went to its daemon went with it: an interrupt it had yet to answer ends
            // the client as it asked, its run never made; the rest wait for the next
            // daemon's connection.
            Ran::Restart => {
                if let Some(sig) = let_go(current) {
                    end_by(sig);
                }
            }
            Ran::Failed(e) => {
                if let Some(sig) = let_go(current) {
                    end_by(sig);
                }
                return failed(&e);
            }
        }
    }
    failed("the daemon kept asking for a restart")
}

/// Where a run's signals go, from the moment its request is on its way, and what went
/// there (D26). Its command may run, and say so on this client's stdout, before this client
/// has heard that its run was made (CREATED): a signal taken then had no connection to go
/// on, and one the client was started ignoring went nowhere while the command waited for
/// it (2 hours in budget E2E 40170179004's `signals_reach_a_warm_run`, PM M332). Each goes
/// on the run's connection now, and its daemon says which side of the run's making it came.
#[derive(Debug)]
struct Current {
    says: Option<Says>,
    /// Its run is made, or needs no making (an exec's, an attach's): what it sends is the
    /// command's.
    made: bool,
    /// The first INTERRUPT it sent before it heard its run was made.
    interrupted: Option<Sig>,
}

impl Current {
    const fn new() -> Current {
        Current {
            says: None,
            made: false,
            interrupted: None,
        }
    }
}

fn lock_current(current: &Mutex<Current>) -> std::sync::MutexGuard<'_, Current> {
    current.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The run's connection, its request on its way there: what this client sends goes on it
/// from now.
fn hold(current: &Mutex<Current>, to: Says) {
    let mut c = lock_current(current);
    c.says = Some(to);
    c.made = false;
    c.interrupted = None;
}

/// Its run made, or one needing no making: what the client sends is its command's, whose
/// terminal is sized as it starts, in case that changed since the request; an attach's one
/// row and column larger first, so that the size changes and the command redraws what it
/// shows (docker/cli attach.go resizeTTY).
fn made(current: &Mutex<Current>, resizes: bool, attach: bool) {
    let mut c = lock_current(current);
    c.made = true;
    c.interrupted = None;
    if resizes && let Some(to) = &c.says {
        if attach {
            resize_by(to, 1);
        }
        resize(to);
    }
}

/// Lets go of the run's connection, whose daemon went or asked for a restart: says the
/// INTERRUPT the client sent before its run was made, which none will answer now.
fn let_go(current: &Mutex<Current>) -> Option<Sig> {
    let mut c = lock_current(current);
    c.says = None;
    c.interrupted.take()
}

/// Sends Linux signal `linux` on the run's connection in `current`, as INTERRUPT where it
/// would end the client (`ending`, the signal), else as SIGNAL: whether it went. With
/// `resize_first`, the command's terminal is resized first (SIGWINCH).
fn hand(current: &Mutex<Current>, linux: u32, ending: Option<Sig>, resize_first: bool) -> bool {
    let mut c = lock_current(current);
    let Some(conn) = &c.says else {
        return false;
    };
    if resize_first {
        resize(conn);
    }
    let kind = if ending.is_some() {
        kind::INTERRUPT
    } else {
        kind::SIGNAL
    };
    if conn.send(kind, &linux.to_be_bytes()).is_err() {
        return false;
    }
    if !c.made && c.interrupted.is_none() {
        c.interrupted = ending;
    }
    true
}

/// Ends this client as signal `sig` ends the Docker CLI before its container is made, the
/// terminal restored first: SIGINT and SIGTERM with 128 and their number and nothing said
/// (docker/cli cmd/docker/docker.go, notifyContext); SIGHUP and SIGQUIT as they would have.
/// On Windows the console's events are SIGINT and SIGTERM alone ([`console_signal`]), and
/// Go numbers them so there too, as the Docker CLI exits by them.
fn end_by(sig: Sig) -> ! {
    terminal::restore();
    #[cfg(unix)]
    if sig != libc::SIGINT && sig != libc::SIGTERM {
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
    std::process::exit(128 + sig)
}

/// Where a client's messages to its command go: its slot's `say` (D185), or the socket of
/// a daemon of a build from before slots.
#[derive(Debug)]
enum Says {
    Slot(shards_ipc::slots::Sender),
    #[cfg(unix)]
    Socket(UnixStream),
}

impl Says {
    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        match self {
            Says::Slot(say) => say.send(kind, payload),
            #[cfg(unix)]
            Says::Socket(conn) => shards_ipc::send(conn, kind, payload, &[]),
        }
    }
}

/// How a run's, an exec's or an attach's client ends.
enum Ran {
    /// With the command's status, and its timing line where it asked for one.
    Ended(ExitCode, Option<String>),
    /// Asked to ask again: a daemon of another build stepped aside.
    Restart,
    Failed(String),
}

/// The command's messages, as `recv` takes them, until it ends: its ID once made, for
/// `--cidfile`, from when what the client sends on the run's connection in `current` is
/// the command's, and its terminal follows this client's (a run's once the daemon has made
/// its container, an exec's at once); a detached run's answer; its status; or the daemon's
/// answer to the client's asking to be ended before its run was made.
fn ran_on(
    request: &Attached,
    home: &Path,
    cid: &mut Option<crate::cli::request::CidFile>,
    current: &Mutex<Current>,
    resizes: bool,
    recv: &mut dyn FnMut() -> io::Result<Option<shards_ipc::Message>>,
) -> Ran {
    let attach = request.kind == kind::ATTACH;
    if request.kind != kind::START {
        made(current, resizes, attach);
    }
    loop {
        match recv() {
            // Made: its ID, for `--cidfile`.
            Ok(Some(m)) if m.kind == kind::CREATED => {
                if let Some(cid) = cid
                    && let Err(e) = cid.write(&String::from_utf8_lossy(&m.payload))
                {
                    return Ran::Failed(e);
                }
                made(current, resizes, attach);
            }
            // Its INTERRUPT, taken before its run was made (D26).
            Ok(Some(m)) if m.kind == kind::INTERRUPTED => {
                let sig = m
                    .payload
                    .first_chunk::<4>()
                    .and_then(|linux| shards_ipc::local_signal(u32::from_be_bytes(*linux)))
                    .unwrap_or(SIGINT);
                end_by(sig);
            }
            Ok(Some(m)) if m.kind == kind::EXIT => {
                let Some((&status, timing)) = m.payload.split_first() else {
                    return Ran::Failed("the command's microVM sent no status".into());
                };
                let timing = (!timing.is_empty()).then(|| with_client_rss(&String::from_utf8_lossy(timing)));
                // A microVM given a host's devices (D154) sends its process's pidfd, or on
                // Windows its process's handle: the run is over once that has ended, and the
                // devices are free. (Through a slot, its daemon waits for it before it says
                // the run's end.)
                if let Some(process) = m.fds.first() {
                    ended(process);
                }
                return Ran::Ended(ExitCode::from(status), timing);
            }
            Ok(Some(m)) if m.kind == kind::RESTART => return Ran::Restart,
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
                return Ran::Ended(
                    ExitCode::from(m.payload.first().copied().unwrap_or(NOT_RUN)),
                    None,
                );
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                return Ran::Failed(format!(
                    "the command's microVM stopped before the command ended; see {}",
                    log(home).display()
                ));
            }
        }
    }
}

/// Shows what the command writes to its slot's `from`, its stdout's or stderr's, on `to`,
/// this client's own, as it comes and unbuffered: a terminal shows a prompt with no newline
/// at once.
fn show(from: &File, mut to: Box<dyn Write + Send>) -> io::Result<u64> {
    io::copy(&mut shards_ipc::slots::Reader(from), &mut to)
}

/// This process's stdout (1) or stderr (2), for what the command writes to it as it comes:
/// on Unix a descriptor of its own, which writes unbuffered; on Windows std's own, which
/// writes a console in UTF-16 (WriteConsoleW), as a console takes text whatever its code
/// page, each write flushed.
fn own_stdio(which: u8) -> io::Result<Box<dyn Write + Send>> {
    #[cfg(unix)]
    {
        let fd = if which == 2 {
            io::stderr().as_fd().try_clone_to_owned()?
        } else {
            io::stdout().as_fd().try_clone_to_owned()?
        };
        Ok(Box::new(File::from(fd)))
    }
    #[cfg(windows)]
    {
        Ok(if which == 2 {
            Box::new(Flushed(io::stderr()))
        } else {
            Box::new(Flushed(io::stdout()))
        })
    }
}

/// A writer each of whose writes is flushed at once: a terminal shows a prompt with no
/// newline as it comes.
#[cfg(windows)]
struct Flushed<W: Write>(W);

#[cfg(windows)]
impl<W: Write> Write for Flushed<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.0.write(buf)?;
        self.0.flush()?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Waits for the process `process`, its handle, to end. Whatever stops the wait short, it is
/// over.
#[cfg(windows)]
fn ended(process: &shards_ipc::fd::OwnedFd) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
    // SAFETY: a process handle the message holds, waited for.
    unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
}

/// Waits for the process whose pidfd `pidfd` is to end: it polls readable then
/// (pidfd_open(2)). Whatever stops the wait short, it is over.
#[cfg(unix)]
fn ended(pidfd: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd as _;
    let mut poll = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: poll(2) of one pollfd this function owns, with no timeout.
        let r = unsafe { libc::poll(&raw mut poll, 1, -1) };
        if r >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

/// A descriptor a container command's request stands for, and which way it goes: what the
/// daemon writes (`To`: save's and export's output, cp's archive) or reads (`From`:
/// import's and load's input, cp's extract).
#[derive(Debug, Clone, Copy)]
pub enum Stream<'a> {
    To(BorrowedFd<'a>),
    From(BorrowedFd<'a>),
}

impl Stream<'_> {
    pub fn fd(&self) -> BorrowedFd<'_> {
        match self {
            Stream::To(fd) | Stream::From(fd) => *fd,
        }
    }
}

/// Where a request went: a slot (D185), or the socket of a daemon of a build from before
/// slots, which asked there steps aside for this client's.
enum Asked {
    Slot(shards_ipc::slots::Client),
    #[cfg(unix)]
    Socket(UnixStream),
}

/// Asks `home`'s daemon, whose binary is `daemon`, through a slot (D185): `kind` and
/// `payload`, the slot's `streams` opened, and the descriptors the request stands for
/// named (`named`, of `slots::fds`). Starts the daemon where none offers slots, unless
/// `started` says this process has, `waiting` first, and waits for its first slot as long
/// as a daemon takes to start.
#[allow(clippy::too_many_arguments)]
fn ask_slot(
    home: &Path,
    daemon: &Path,
    started: &mut bool,
    kind: u8,
    payload: &[u8],
    streams: u8,
    named: &[u8],
    waiting: &mut dyn FnMut() -> Result<(), String>,
) -> Result<Asked, String> {
    use shards_ipc::slots;
    let mut deadline = Instant::now() + START_TIMEOUT;
    loop {
        // The home is this process's working directory (`enter`): its slots are named from
        // there, as its socket is.
        match slots::take(Path::new("."), streams) {
            Ok(slots::Taken::Slot(client)) => match client.ask(kind, payload, named) {
                Ok(()) => return Ok(Asked::Slot(client)),
                // Removed as it was asked: the next.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
                Err(e) => return Err(format!("asking the daemon: {e}")),
            },
            Ok(slots::Taken::NoDaemon) => {
                #[cfg(unix)]
                if let Ok(conn) = UnixStream::connect(SOCKET) {
                    return Ok(Asked::Socket(conn));
                }
                // What follows may wait as long as a daemon takes to start, or the one it
                // replaces to end its runs: what must not wait with it goes first.
                waiting()?;
                if !*started {
                    start(daemon, home)?;
                    *started = true;
                    deadline = Instant::now() + START_TIMEOUT;
                }
                if shards_ipc::exiting(home) {
                    deadline = Instant::now() + START_TIMEOUT;
                }
                if Instant::now() >= deadline {
                    return Err(format!("the daemon did not start; see {}", log(home).display()));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(format!("{}: {e}", home.join(slots::DIR).display())),
        }
    }
}

/// The slot's streams `fds` stand for, and their order as the request names them: what the
/// daemon writes its `out`, what it reads its `in`.
fn streams_of(fds: &[Stream<'_>]) -> (u8, Vec<u8>) {
    use shards_ipc::slots;
    fds.iter().fold((0, Vec::new()), |(opened, mut named), s| {
        let (bit, fd) = match s {
            Stream::To(_) => (slots::streams::OUT, slots::fds::OUT),
            Stream::From(_) => (slots::streams::IN, slots::fds::IN),
        };
        named.push(fd);
        (opened | bit, named)
    })
}

/// The daemon's next message on its slot, as its socket's would be, without descriptors.
fn slot_message(client: &shards_ipc::slots::Client) -> io::Result<Option<shards_ipc::Message>> {
    Ok(client.recv()?.map(|f| shards_ipc::Message {
        kind: f.kind,
        payload: f.payload,
        fds: Vec::new(),
    }))
}

/// Copies `client`'s streams to and from `fds`, the descriptors its request stands for:
/// what the daemon writes, from its slot's `out` to the descriptor, on a thread of
/// `scope`'s, which its caller waits for; what it reads, from the descriptor to `in`, on a
/// thread of its own, which ends as its input does, or with this process, where the
/// daemon has done before that input ends (a terminal's).
fn copy_streams<'s>(
    scope: &'s std::thread::Scope<'s, '_>,
    client: &mut shards_ipc::slots::Client,
    fds: &[Stream<'_>],
) -> Result<Vec<std::thread::ScopedJoinHandle<'s, io::Result<u64>>>, String> {
    let mut waited = Vec::new();
    for stream in fds {
        let fd = File::from(
            stream
                .fd()
                .try_clone_to_owned()
                .map_err(|e| format!("a stream's descriptor: {e}"))?,
        );
        match stream {
            Stream::To(_) => {
                let Some(out) = client.output.take() else {
                    continue;
                };
                let copied = std::thread::Builder::new()
                    .name("shards-out".into())
                    .spawn_scoped(scope, move || {
                        let mut to = fd;
                        io::copy(&mut shards_ipc::slots::Reader(&out), &mut to)
                    })
                    .map_err(|e| format!("a stream's thread: {e}"))?;
                waited.push(copied);
            }
            Stream::From(_) => {
                let Some(mut input) = client.input.take() else {
                    continue;
                };
                std::thread::Builder::new()
                    .name("shards-in".into())
                    .spawn(move || io::copy(&mut shards_ipc::Polled(&fd), &mut input))
                    .map_err(|e| format!("a stream's thread: {e}"))?;
            }
        }
    }
    Ok(waited)
}

/// Runs a container command (`ps`, `wait`, `rm`, ...) in the daemon of `home`, whose
/// binary is `daemon`, printing what the daemon answers as it comes, and exits with the
/// command's status.
pub fn container(home: &Path, daemon: &Path, command: &Command, fds: &[Stream<'_>]) -> u8 {
    let failed = |message: &str| {
        let _ = writeln!(io::stderr(), "shards: {message}");
        NOT_RUN
    };
    let mut started = match enter(home, daemon) {
        Ok(started) => started,
        Err(e) => return failed(&e),
    };
    // A daemon from another build answers RESTART once it has stepped aside.
    for _ in 0..RESTARTS {
        let (opened, named) = streams_of(fds);
        let asked = match ask_slot(
            home,
            daemon,
            &mut started,
            kind::CONTAINER,
            &command.encode(),
            opened,
            &named,
            &mut || Ok(()),
        ) {
            Ok(asked) => asked,
            Err(e) => return failed(&e),
        };
        let answered = match asked {
            Asked::Slot(mut client) => std::thread::scope(|scope| {
                let copies = copy_streams(scope, &mut client, fds)?;
                let status = answer(scope, &mut || slot_message(&client), home, command.east_asian);
                for copy in copies {
                    if let Ok(Err(e)) = copy.join() {
                        return Err(format!("a stream of the daemon's: {e}"));
                    }
                }
                Ok(status)
            }),
            #[cfg(unix)]
            Asked::Socket(conn) => {
                let raw: Vec<BorrowedFd<'_>> = fds.iter().map(Stream::fd).collect();
                match shards_ipc::send(&conn, kind::CONTAINER, &command.encode(), &raw) {
                    Err(e) if unread(&e) => continue,
                    Err(e) => return failed(&format!("asking the daemon: {e}")),
                    Ok(()) => {}
                }
                Ok(std::thread::scope(|scope| {
                    answer(scope, &mut || shards_ipc::recv(&conn), home, command.east_asian)
                }))
            }
        };
        match answered {
            Ok(Some(status)) => return status,
            Ok(None) => {}
            Err(e) => return failed(&e),
        }
    }
    failed("the daemon kept asking for a restart")
}

/// Asks the daemon of `home`, whose binary is `daemon`, for `command` with `fds`, and
/// takes its answer whole: its status, and what it said on stdout and on stderr.
pub fn ask(
    home: &Path,
    daemon: &Path,
    command: &Command,
    fds: &[Stream<'_>],
) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
    let mut started = enter(home, daemon)?;
    // A daemon from another build answers RESTART once it has stepped aside.
    for _ in 0..RESTARTS {
        let (opened, named) = streams_of(fds);
        let answered = match ask_slot(
            home,
            daemon,
            &mut started,
            kind::CONTAINER,
            &command.encode(),
            opened,
            &named,
            &mut || Ok(()),
        )? {
            Asked::Slot(client) => answered_whole(client, fds)?,
            #[cfg(unix)]
            Asked::Socket(conn) => {
                let raw: Vec<BorrowedFd<'_>> = fds.iter().map(Stream::fd).collect();
                match shards_ipc::send(&conn, kind::CONTAINER, &command.encode(), &raw) {
                    Err(e) if unread(&e) => continue,
                    Err(e) => return Err(format!("asking the daemon: {e}")),
                    Ok(()) => {}
                }
                collected(&mut || shards_ipc::recv(&conn))?
            }
        };
        if let Some(answered) = answered {
            return Ok(answered);
        }
    }
    Err("the daemon kept asking for a restart".into())
}

/// Asks the daemon whose home is this process's working directory for `command`, with
/// `fds`, and takes its answer whole, as [`ask`] does: what a daemon asks of itself (a
/// context's steps, daemon/remote.rs), which starts no daemon where its own has closed.
// The daemon's, Unix's alone as yet (D195).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn ask_here(command: &Command, fds: &[Stream<'_>]) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
    use shards_ipc::slots;
    for _ in 0..RESTARTS {
        let (opened, named) = streams_of(fds);
        let client = match slots::take(Path::new("."), opened) {
            Ok(slots::Taken::Slot(client)) => client,
            Ok(slots::Taken::NoDaemon) => return Err("the daemon is shutting down".into()),
            Err(e) => return Err(format!("{}: {e}", slots::DIR)),
        };
        match client.ask(kind::CONTAINER, &command.encode(), &named) {
            // Removed as it was asked: the next.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => continue,
            Err(e) => return Err(format!("asking the daemon: {e}")),
            Ok(()) => {}
        }
        if let Some(answered) = answered_whole(client, fds)? {
            return Ok(answered);
        }
    }
    Err("the daemon kept asking for a restart".into())
}

/// A command's answer taken whole: its status, and what it said on stdout and on stderr.
type Whole = (u8, Vec<u8>, Vec<u8>);

/// The daemon's answer on `client`'s slot, its streams copied to and from `fds` as it
/// answers; `None` for a restart.
fn answered_whole(
    mut client: shards_ipc::slots::Client,
    fds: &[Stream<'_>],
) -> Result<Option<Whole>, String> {
    std::thread::scope(|scope| {
        let copies = copy_streams(scope, &mut client, fds)?;
        let answered = collected(&mut || slot_message(&client));
        for copy in copies {
            if let Ok(Err(e)) = copy.join() {
                return Err(format!("a stream of the daemon's: {e}"));
            }
        }
        answered
    })
}

/// The daemon's answer, its messages as `recv` takes them, taken whole; `None` where it
/// asks for a restart.
fn collected(
    recv: &mut dyn FnMut() -> io::Result<Option<shards_ipc::Message>>,
) -> Result<Option<Whole>, String> {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    loop {
        match recv() {
            Ok(Some(m)) if m.kind == kind::OUT => out.extend(m.payload),
            Ok(Some(m)) if m.kind == kind::ERR => err.extend(m.payload),
            Ok(Some(m)) if m.kind == kind::END => {
                return Ok(Some((m.payload.first().copied().unwrap_or(1), out, err)));
            }
            Ok(Some(m)) if m.kind == kind::RESTART => return Ok(None),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Err("the daemon ended before it answered".to_string()),
        }
    }
}

/// The daemon's answer, its messages as `recv` takes them: its output, as it comes, and its
/// status, or `None` when it asks for a restart. Steps of a pull, which the daemon sends a
/// terminal, are shown by a display of their own (show.rs), on a thread of this `scope`;
/// the daemon's text then goes through it, below its frame.
fn answer<'s>(
    scope: &'s std::thread::Scope<'s, '_>,
    recv: &mut dyn FnMut() -> io::Result<Option<shards_ipc::Message>>,
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
    // A listing this client could not lay out (its `--format` failed on a row).
    let mut unlisted = false;
    // A page redrawn in place as the daemon sends it again (`stats`).
    let mut live = shards_tui::frame::Frame::new();
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
        match recv() {
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
                    // A listing's rows, laid out here as the CLI lays them out; else a
                    // page of shards' own.
                    match crate::cli::listing::show(&sheet) {
                        Ok(true) => {}
                        Ok(false) => {
                            crate::cli::screens::show(&sheet, &mut live);
                            paged = true;
                        }
                        Err(e) => {
                            let _ = writeln!(std::io::stderr(), "{e}");
                            unlisted = true;
                        }
                    }
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
                        .map(|l| l.strip_prefix("shards: ").unwrap_or(l))
                        .collect();
                    crate::cli::look::panel(p, &said.join("\n"), !paged);
                }
                if unlisted {
                    return Some(1);
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

/// Whether a request's send failed as on a connection the daemon shut without reading
/// it: it steps aside for another build's client, and the next daemon takes the request.
#[cfg(unix)]
fn unread(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

/// How long `stop` waits for the daemon to hand over the runs in hand and exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// Asks the daemon of `home` to exit once the runs in hand are handed over, and waits
/// until it has: the daemon holds the request's slot open until it exits.
pub fn stop(home: &Path) -> ExitCode {
    use shards_ipc::slots;
    match std::env::set_current_dir(home) {
        Ok(()) => {}
        // No home: no daemon to stop.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ExitCode::SUCCESS,
        Err(e) => return failed(&format!("{}: {e}", home.display())),
    }
    for _ in 0..RESTARTS {
        let client = match slots::take(Path::new("."), 0) {
            Ok(slots::Taken::Slot(client)) => client,
            // No slot offered: a daemon of a build from before slots, or none.
            #[cfg(unix)]
            Ok(slots::Taken::NoDaemon) => return stop_socket(home),
            // No slot offered: no daemon, Windows' first having slots.
            #[cfg(windows)]
            Ok(slots::Taken::NoDaemon) => return ExitCode::SUCCESS,
            Err(e) => return failed(&format!("{}: {e}", home.join(slots::DIR).display())),
        };
        match client.ask(kind::STOP, &[], &[]) {
            // Removed as it was asked: the next.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => continue,
            Err(e) => return failed(&format!("asking the daemon to stop: {e}")),
            Ok(()) => {}
        }
        return match client.recv_within(STOP_TIMEOUT) {
            // Its end. Closing already, or stepping aside for another build's client, it
            // exits all the same.
            Ok(None) => ExitCode::SUCCESS,
            Ok(Some(f)) if f.kind == kind::RESTART => ExitCode::SUCCESS,
            Err(e) if e.kind() != io::ErrorKind::TimedOut => ExitCode::SUCCESS,
            Ok(Some(_)) | Err(_) => failed(&format!("the daemon did not exit within {STOP_TIMEOUT:?}")),
        };
    }
    failed("the daemon kept asking for a restart")
}

/// [`stop`] of a daemon of a build from before slots (D185), through its socket.
#[cfg(unix)]
fn stop_socket(home: &Path) -> ExitCode {
    match UnixStream::connect(SOCKET) {
        Ok(conn) => {
            if let Err(e) = shards_ipc::send(&conn, kind::STOP, &[], &[]) {
                return failed(&format!("asking the daemon to stop: {e}"));
            }
            if let Err(e) = conn.set_read_timeout(Some(STOP_TIMEOUT)) {
                return failed(&format!("the daemon's connection: {e}"));
            }
            match shards_ipc::recv(&conn) {
                // Stepping aside for another build's client, it exits all the same.
                Ok(None) => ExitCode::SUCCESS,
                Ok(Some(m)) if m.kind == kind::RESTART => ExitCode::SUCCESS,
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
/// first field, `client_rss_kib`, and its microseconds from its `main` to now,
/// `client_us`, for benchmarks: a run timed from its client's spawn, less those, is its
/// client's launch, which its host may hold back (PM M166).
fn with_client_rss(timing: &str) -> String {
    let kib = shards_ipc::peak_rss_kib();
    let us = shards_vmm::log::uptime_us();
    match timing.strip_prefix('{') {
        Some(rest) if rest.trim_start().starts_with('}') => {
            format!("{{\"client_rss_kib\":{kib},\"client_us\":{us}{rest}")
        }
        Some(rest) => format!("{{\"client_rss_kib\":{kib},\"client_us\":{us},{rest}"),
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

/// Runs `daemon daemon --detached`, which starts the daemon in the background and exits;
/// the daemon creates its home and writes its messages to the log there. Of daemons
/// started at once, one takes the home's lock and the rest exit. What the starter says
/// when it fails, a setting the daemon cannot keep, is what this says.
fn start(daemon: &Path, home: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    #[cfg(windows)]
    let null = std::fs::File::options()
        .read(true)
        .write(true)
        .open("NUL")
        .map_err(|e| format!("NUL: {e}"))?;
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
#[cfg(unix)]
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
/// as a container outlives the `docker run` that detached from it. On Windows a console is
/// read for its records ([`terminal::ConsoleInput`]), which say its resizes too: with
/// `resizes` each goes on the run's connection in `current`, as SIGWINCH does on Unix.
fn fill_stdin(
    mut writer: File,
    mut proxy: Option<EscapeProxy>,
    detach_said: Option<(&'static str, u8)>,
    current: &'static Mutex<Current>,
    resizes: bool,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            #[cfg(unix)]
            let take = {
                let _ = (current, resizes);
                let stdin = io::stdin().lock();
                move |buf: &mut [u8]| shards_ipc::read(&stdin, buf)
            };
            #[cfg(windows)]
            let mut take = {
                let mut console = terminal::ConsoleInput::new(move || {
                    if resizes && let Some(conn) = lock_current(current).says.as_ref() {
                        resize(conn);
                    }
                });
                move |buf: &mut [u8]| match console.as_mut() {
                    Some(console) => console.read(buf),
                    None => io::stdin().read(buf),
                }
            };
            loop {
                let read = match take(&mut buf) {
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
                    let code = match detach_said {
                        Some((said, code)) => {
                            let _ = writeln!(io::stderr(), "{said}");
                            code
                        }
                        None => 0,
                    };
                    std::process::exit(i32::from(code));
                }
            }
        })
        .map(drop)
        .map_err(|e| format!("stdin thread: {e}"))
}

/// Sends the size of this process's stdout as the command's terminal's.
fn resize(conn: &Says) {
    resize_by(conn, 0);
}

/// Sends the size of this process's stdout, `more` rows and columns larger, as the
/// command's terminal's.
fn resize_by(conn: &Says, more: u16) {
    let (rows, cols) = terminal::size(1);
    let (rows, cols) = (rows.saturating_add(more), cols.saturating_add(more));
    if rows != more && cols != more {
        let [a, b] = rows.to_be_bytes();
        let [c, d] = cols.to_be_bytes();
        let _ = conn.send(kind::RESIZE, &[a, b, c, d]);
    }
}

/// Takes the signals `docker run` forwards to the command, for [`forward`] to send over the
/// current connection, even those this process was started ignoring, as the Docker CLI
/// does (shards_ipc::take_forwarded). They are blocked in the calling thread, which every
/// thread started later inherits, so only the forwarder's `sigwait` receives them. One that
/// would end the client, unless it was ignored, goes as INTERRUPT, which its daemon takes
/// as the client's end before its run is made (D26); with no connection to send it on, the
/// request yet to go, it ends the client ([`end_by`]). With `reads_terminal`, the
/// terminal's job control applies to the client (shards_ipc::forwarded). With `resizes`,
/// SIGWINCH first resizes the command's terminal, then goes to the command too, as both
/// reach a Docker container's (docker/cli tty.go, signals.go). Without `proxies`, only
/// SIGWINCH is taken, to resize the command's terminal with `resizes`, and the rest act on
/// the client as they would.
#[cfg(unix)]
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
#[cfg(unix)]
struct Signals {
    set: libc::sigset_t,
    ignored: Vec<libc::c_int>,
    resizes: bool,
    proxies: bool,
}

/// Starts the thread that receives `signals`, as [`take_signals`] says, on the run's
/// connection in `current` once there is one.
#[cfg(unix)]
fn forward(signals: Option<Signals>, current: &'static Mutex<Current>) -> Result<(), String> {
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
                    if let Some(conn) = lock_current(current).says.as_ref() {
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
                    for linux in shards_ipc::sent_faults() {
                        hand(current, linux, None, false);
                    }
                    continue;
                }
                let Some(linux) = shards_ipc::linux_signal(sig) else {
                    continue;
                };
                let ends = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&sig)
                    && !ignored.contains(&sig);
                let ending = ends.then_some(sig);
                if !hand(current, linux, ending, resizes && sig == libc::SIGWINCH) && ends {
                    end_by(sig);
                }
            }
        })
        .map_err(|e| format!("signal thread: {e}"))?;
    Ok(())
}

/// What Windows' [`forward`] does: with `proxies`, the console's events go on the run's
/// connection; with `resizes`, its size follows the console's.
#[cfg(windows)]
struct Signals {
    reads_terminal: bool,
    resizes: bool,
    proxies: bool,
}

/// Windows' [`take_signals`]: nothing to block, the console's events coming to a handler
/// ([`on_console_event`]) on a thread of the system's.
#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "Unix's, which can fail, has the same signature"
)]
fn take_signals(reads_terminal: bool, resizes: bool, proxies: bool) -> Result<Option<Signals>, String> {
    Ok((proxies || resizes).then_some(Signals {
        reads_terminal,
        resizes,
        proxies,
    }))
}

/// The run's connection the console's events go on ([`on_console_event`]): the client's one
/// [`Current`], set as it starts to forward them.
#[cfg(windows)]
static CONSOLE_CURRENT: std::sync::OnceLock<&'static Mutex<Current>> = std::sync::OnceLock::new();

/// The signal a console event is taken as, as Go's runtime takes them, which the Docker CLI's
/// forwarding sees (src/runtime/os_windows.go, ctrlHandler): Ctrl-C and Ctrl-Break SIGINT;
/// the console's close, the user's logoff and the system's shutdown SIGTERM; any other none.
#[cfg(windows)]
fn console_signal(event: u32) -> Option<Sig> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => Some(shards_ipc::signal::SIGINT),
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => Some(shards_ipc::signal::SIGTERM),
        _ => None,
    }
}

/// The console's handler: an event goes on the run's connection as its signal does
/// ([`hand`], INTERRUPT, each ending the client), or, with none to go on, ends the client as
/// that signal does ([`end_by`]). In raw mode Ctrl-C is a key the command reads, as on Unix;
/// Ctrl-Break stays an event.
#[cfg(windows)]
unsafe extern "system" fn on_console_event(event: u32) -> windows_sys::core::BOOL {
    let (Some(sig), Some(current)) = (console_signal(event), CONSOLE_CURRENT.get()) else {
        return 0;
    };
    if !hand(current, u32::try_from(sig).unwrap_or(0), Some(sig), false) {
        end_by(sig);
    }
    1
}

/// Windows' [`forward`]: with `proxies`, the console's events go to [`on_console_event`];
/// with `resizes`, where no stdin filler reads the console (a `-t` run without `-i`), its
/// records are read on a thread of their own for the resizes they say, its keys going to no
/// command.
#[cfg(windows)]
fn forward(signals: Option<Signals>, current: &'static Mutex<Current>) -> Result<(), String> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    let Some(Signals {
        reads_terminal,
        resizes,
        proxies,
    }) = signals
    else {
        return Ok(());
    };
    if proxies {
        let _ = CONSOLE_CURRENT.set(current);
        // SAFETY: a handler this process keeps for its life.
        if unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) } == 0 {
            return Err(format!(
                "taking the console's events: {}",
                io::Error::last_os_error()
            ));
        }
    }
    if resizes
        && !reads_terminal
        && let Some(mut console) = terminal::ConsoleInput::new(move || {
            if let Some(conn) = lock_current(current).says.as_ref() {
                resize(conn);
            }
        })
    {
        std::thread::Builder::new()
            .name("resizes".into())
            .spawn(move || {
                let mut buf = [0u8; 256];
                while console.read(&mut buf).is_ok_and(|n| n > 0) {}
            })
            .map_err(|e| format!("resize thread: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The console's events are the signals Go's runtime takes them as, and no others.
    #[cfg(windows)]
    #[test]
    fn console_events_are_go_runtimes_signals() {
        use windows_sys::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        };
        assert_eq!(console_signal(CTRL_C_EVENT), Some(2));
        assert_eq!(console_signal(CTRL_BREAK_EVENT), Some(2));
        for event in [CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT] {
            assert_eq!(console_signal(event), Some(15));
        }
        assert_eq!(console_signal(3), None);
    }

    /// Once its request is on its way, a client's signals go on its run's connection, one
    /// that would end it as INTERRUPT, which its daemon takes as its end before its run is
    /// made (D26); before, none goes, for the client to end itself. The first interrupt
    /// before its run is made is kept, for a daemon that goes without answering it, and is
    /// let go of once the run is made. Taken before it heard its run was made, a signal had
    /// no connection to go on, and one the client was started ignoring went nowhere while
    /// the command waited for it (PM M332).
    #[cfg(unix)]
    #[test]
    fn a_runs_signals_go_on_its_connection_from_its_request() {
        let current = Mutex::new(Current::new());
        assert!(
            !hand(&current, 2, Some(libc::SIGINT), false),
            "sent before the request"
        );
        let (ours, theirs) = UnixStream::pair().unwrap();
        hold(&current, Says::Socket(ours));
        assert!(hand(&current, 10, None, false));
        assert!(hand(&current, 2, Some(libc::SIGINT), false));
        assert!(hand(&current, 15, Some(libc::SIGTERM), false));
        for (kind, linux) in [(kind::SIGNAL, 10u32), (kind::INTERRUPT, 2), (kind::INTERRUPT, 15)] {
            let m = shards_ipc::recv(&theirs).unwrap().unwrap();
            assert_eq!((m.kind, m.payload), (kind, linux.to_be_bytes().to_vec()));
        }
        assert_eq!(let_go(&current), Some(libc::SIGINT));
        assert!(
            !hand(&current, 2, Some(libc::SIGINT), false),
            "sent once let go of"
        );
        let (ours, theirs) = UnixStream::pair().unwrap();
        hold(&current, Says::Socket(ours));
        assert!(hand(&current, 1, Some(libc::SIGHUP), false));
        made(&current, false, false);
        assert!(hand(&current, 2, Some(libc::SIGINT), false));
        assert_eq!(let_go(&current), None, "kept once its run was made");
        for linux in [1u32, 2] {
            let m = shards_ipc::recv(&theirs).unwrap().unwrap();
            assert_eq!(
                (m.kind, m.payload),
                (kind::INTERRUPT, linux.to_be_bytes().to_vec())
            );
        }
    }

    /// The client's own fields go first in the VM's timing line, an empty one too, and a
    /// line that is no object stays as it came.
    #[test]
    fn the_timing_line_leads_with_the_clients_own_fields() {
        let line = with_client_rss(r#"{"released_us":5}"#);
        let fields: Vec<&str> = line
            .trim_matches(|c| c == '{' || c == '}')
            .split(',')
            .map(|f| f.split(':').next().unwrap())
            .collect();
        assert_eq!(
            fields,
            [r#""client_rss_kib""#, r#""client_us""#, r#""released_us""#]
        );
        assert!(with_client_rss("{}").ends_with('}'));
        assert!(!with_client_rss("{}").contains(",}"));
        assert_eq!(with_client_rss("not json"), "not json");
    }
}
