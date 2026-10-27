//! Contexts (D180): another machine's shards, which this one's commands run on whole, over
//! QUIC. A home keeps the contexts it joined (`contexts/NAME`) and the one its commands use
//! (`context`); the machine that runs the microVMs keeps the clients it enrolled
//! (`clients/NAME`) and the invitations it made (`invitations/NAME`), and its daemon, where it
//! listens, its address (`listening`). Each a short text, readable by its user alone.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use shards_remote::client::{self, Remote};
use shards_remote::frame::{self, Request};
use shards_remote::identity::{self, Identity};
use shards_remote::invite::{self, Invitation};

/// The exit status of a command shards did not run: an error before its command ran, the
/// status a `docker` script reads for one.
const NOT_RUN: u8 = 125;

/// What names a connection to the machine's Engine API (D180 part 4) to its daemon: no
/// command line's word, as no command line can hold NUL.
pub(crate) const ENGINE_STEP: &str = "\u{0}engine";

/// Where this home's daemon serves context `name`'s engine, relative to the home: in `e`, by
/// its name hashed, so that the path of any name fits a socket's address (sun_path, 104
/// bytes on macOS): 71 in a macOS home of a user of nine letters.
pub(crate) fn engine_socket(name: &str) -> PathBuf {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(name.as_bytes());
    let short: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    Path::new("e").join(format!("{short}.sock"))
}

/// Context `name`'s engine as `DOCKER_HOST` names one.
fn engine_endpoint(home: &Path, name: &str) -> String {
    format!("unix://{}", home.join(engine_socket(name)).display())
}

/// The variable a daemon sets for a client's command on a terminal of its own: the command
/// takes the terminal as its session's controlling terminal, first thing, as a login's
/// command has its own (D180), which no action of posix_spawn(3) makes on every system.
#[cfg(unix)]
pub(crate) const TAKE_TERMINAL: &str = "SHARDS_TAKE_TERMINAL";

/// What a client says in place of a run's or create's `--cidfile` path, before it: the file is
/// the client's, made and written there, the ID told it by its machine (`frame::Tag::Made`).
pub(crate) const CIDFILE: &str = "\0cidfile/";

/// The variable a daemon sets for a client's command whose `--cidfile` is the client's
/// ([`CIDFILE`]): the descriptor the command writes its microVM's ID to, which the daemon
/// tells the client.
#[cfg(unix)]
pub(crate) const CIDFILE_FD: &str = "SHARDS_CIDFILE_FD";

/// The descriptor [`CIDFILE_FD`] names.
#[cfg(unix)]
pub(crate) const CIDFILE_DESCRIPTOR: i32 = 3;

/// The descriptor [`take_cidfile`] took.
#[cfg(unix)]
static TOLD: std::sync::Mutex<Option<fs::File>> = std::sync::Mutex::new(None);

/// Takes what [`CIDFILE_FD`] names, close-on-exec, and lets the variable go, so that nothing
/// this process starts takes it too.
#[cfg(unix)]
pub(crate) fn take_cidfile() {
    use std::os::fd::FromRawFd as _;
    let Some(fd) = std::env::var(CIDFILE_FD).ok().and_then(|v| v.parse::<i32>().ok()) else {
        return;
    };
    // SAFETY: first thing in main, before any thread: the environment is this one's alone.
    unsafe { std::env::remove_var(CIDFILE_FD) };
    // SAFETY: fcntl(2) of a descriptor past stdio's, which its daemon gave this process.
    if fd <= 2 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return;
    }
    // SAFETY: the descriptor its daemon gave this process for the ID, which nothing else
    // here holds.
    let file = unsafe { fs::File::from_raw_fd(fd) };
    *TOLD.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
}

/// The descriptor a client's `--cidfile` is told through ([`take_cidfile`]), once.
#[cfg(unix)]
pub(crate) fn told_cidfile() -> Option<fs::File> {
    TOLD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

/// Whether this process's `--cidfile` is a client's ([`take_cidfile`]).
#[cfg(unix)]
pub(crate) fn tells_cidfile() -> bool {
    TOLD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

/// Does what [`TAKE_TERMINAL`] asks, and lets the variable go, so that nothing this process
/// starts takes the terminal too.
#[cfg(unix)]
pub(crate) fn take_terminal() {
    if std::env::var_os(TAKE_TERMINAL).is_none() {
        return;
    }
    // SAFETY: first thing in main, before any thread: the environment is this one's alone.
    unsafe { std::env::remove_var(TAKE_TERMINAL) };
    // SAFETY: isatty(3) on this process's stdio.
    if let Some(fd) = (0..3).find(|&fd| unsafe { libc::isatty(fd) } == 1) {
        // SAFETY: TIOCSCTTY on our own descriptor: a session's leader with no controlling
        // terminal takes it.
        unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, 0) };
    }
}

/// This client's terminal, where it has one: which of its stdin, stdout and stderr it is,
/// and its size.
#[cfg(unix)]
fn here_terminal() -> Option<frame::Terminal> {
    // SAFETY: isatty(3) on this process's stdio.
    let on = |fd| unsafe { libc::isatty(fd) } == 1;
    let first = (0..3).find(|&fd| on(fd))?;
    let (rows, cols) = crate::cli::terminal::size(first);
    Some(frame::Terminal {
        stdin: on(0),
        stdout: on(1),
        stderr: on(2),
        rows,
        cols,
    })
}

/// What steers the command a context runs, while it runs: where this client's terminal's
/// size and signals go.
static STEERED: std::sync::Mutex<Option<client::Controls>> = std::sync::Mutex::new(None);

/// The file this client writes for the command there until it is whole ([`Named`]), which
/// a signal that ends the client removes first: none is left behind.
static UNFINISHED: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Removes the file [`UNFINISHED`] names, for a client that ends.
fn remove_unfinished() {
    let unfinished = UNFINISHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(path) = unfinished {
        let _ = fs::remove_file(path);
    }
}

/// The file here a command's output is written to, which sends `failed` what a write that
/// failed said: a file that is not whole is kept by no exit status.
struct Recorded {
    file: fs::File,
    failed: std::sync::mpsc::Sender<String>,
}

impl io::Write for Recorded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes).inspect_err(|e| {
            if e.kind() != io::ErrorKind::Interrupted {
                let _ = self.failed.send(crate::cli::save::go(e));
            }
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// This client's stdout, which says when its reader has gone: the command, ended there, ends
/// here with status 1, unsaid, as one does here whose reader hung up (daemon/commands.rs,
/// `undelivered`).
struct Printed {
    out: io::Stdout,
    gone: std::sync::mpsc::Sender<()>,
}

impl Printed {
    fn seen<T>(&self, done: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &done
            && matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
            )
        {
            let _ = self.gone.send(());
        }
        done
    }
}

impl io::Write for Printed {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.out.write(bytes);
        self.seen(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let flushed = self.out.flush();
        self.seen(flushed)
    }
}

/// This client's terminal, where it has one: which of its stdin, stdout and stderr its
/// console is, and its size, its window's, which only an output reports.
#[cfg(windows)]
fn here_terminal() -> Option<frame::Terminal> {
    use crate::cli::terminal::{is_console, size};
    let (stdin, stdout, stderr) = (is_console(0), is_console(1), is_console(2));
    if !(stdin || stdout || stderr) {
        return None;
    }
    let (rows, cols) = if stdout {
        size(1)
    } else if stderr {
        size(2)
    } else {
        (0, 0)
    };
    Some(frame::Terminal {
        stdin,
        stdout,
        stderr,
        rows,
        cols,
    })
}

/// A console has no SIGWINCH: a resize is told by the console's record of its screen
/// buffer's size, which its input carries ([`crate::cli::terminal::ConsoleInput`], which
/// reads stdin where it is the console), and sizes the command's terminal as it comes. A
/// console whose window shows part of its buffer (conhost's, with lines kept above it)
/// records no resize that leaves the buffer's size, nor does one whose input is not stdin:
/// there its size is looked at every 250 ms (docker/cli's interval, MonitorTtySize), as
/// Docker's CLI looks at every console's. Its events, ^C and ^Break where its input is not
/// raw, and its closing, go to the command as Go's runtime gives them to a Windows process
/// ([`console_signal`]). The descriptor whose console's size the command's terminal takes,
/// if any: stdout's, else stderr's.
#[cfg(windows)]
fn steer_console(terminal: Option<frame::Terminal>) -> Result<Option<i32>, String> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: a handler of this process's console's events, which it never removes. A
    // process with no console has no events to take: a handler it could not have is none.
    let _ = unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) };
    let Some(terminal) = terminal else {
        return Ok(None);
    };
    let fd = match (terminal.stdout, terminal.stderr) {
        (true, _) => 1,
        (false, true) => 2,
        (false, false) => return Ok(None),
    };
    *SIZED.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((terminal.rows, terminal.cols));
    if terminal.stdin && crate::cli::terminal::shows_whole_buffer(fd) {
        return Ok(Some(fd));
    }
    std::thread::Builder::new()
        .name("resizes".into())
        .spawn(move || {
            while send_size(fd) {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
        .map_err(|e| format!("resize thread: {e}"))?;
    Ok(Some(fd))
}

/// The size last sent to the command (rows, columns): what a resize is told apart from.
#[cfg(windows)]
static SIZED: std::sync::Mutex<Option<(u16, u16)>> = std::sync::Mutex::new(None);

/// Sends the command the console `fd`'s size where it has changed since last sent; a
/// size with no command yet to take it is sent once there is one, as the command's
/// steering is given. False once the command takes no more.
#[cfg(windows)]
fn send_size(fd: i32) -> bool {
    let now = crate::cli::terminal::size(fd);
    let mut sized = SIZED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if *sized == Some(now) {
        return true;
    }
    let steered = STEERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(controls) = steered.as_ref() else {
        return true;
    };
    if controls.resize(now.0, now.1).is_err() {
        return false;
    }
    *sized = Some(now);
    true
}

/// The console's event `kind`: sent to the command as its signal, or, with no command to
/// take it, left to end the client as it would have. Windows ends the process once its
/// closing's handler returns, so that handler waits, as Go's does, for the command's end to
/// end the client first.
#[cfg(windows)]
extern "system" fn on_console_event(kind: u32) -> windows_sys::core::BOOL {
    let Some(linux) = console_signal(kind) else {
        return 0;
    };
    let sent = STEERED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|controls| controls.signal(linux).is_ok());
    if !sent {
        remove_unfinished();
        return 0;
    }
    if linux == 15 {
        loop {
            std::thread::park();
        }
    }
    1
}

/// The Linux number of the signal a console's event `kind` is, as Go's runtime gives it to
/// a Windows process (go1.25.0 runtime/os_windows.go, ctrlHandler), and so docker/cli's
/// signal proxy to a container: ^C and ^Break SIGINT; the console's closing, a logoff and a
/// shutdown SIGTERM.
#[cfg(windows)]
fn console_signal(kind: u32) -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    match kind {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => Some(2),
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => Some(15),
        _ => None,
    }
}

/// Takes the signals this process would act on, which go to the command it forwards
/// instead (`--sig-proxy`). With `terminal`, the
/// descriptor of its terminal, the command's own terminal, SIGWINCH sizes it, as the
/// kernel then signals the command; without, SIGWINCH goes as the rest do. One that comes
/// with no command to take it ends the client as it would have, its terminal restored
/// first (cli/client.rs).
#[cfg(unix)]
fn steer_signals(reads_terminal: bool, terminal: Option<i32>) -> Result<(), String> {
    use std::sync::PoisonError;
    let (mut set, ignored) =
        shards_ipc::take_forwarded(reads_terminal).map_err(|e| format!("taking signals: {e}"))?;
    shards_ipc::take_rest(&mut set).map_err(|e| format!("taking signals: {e}"))?;
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
                let steered = STEERED.lock().unwrap_or_else(PoisonError::into_inner);
                let sent = steered.as_ref().is_some_and(|controls| match terminal {
                    Some(fd) if sig == libc::SIGWINCH => {
                        let (rows, cols) = crate::cli::terminal::size(fd);
                        controls.resize(rows, cols).is_ok()
                    }
                    _ if sig == shards_ipc::CARRIER => {
                        shards_ipc::sent_faults().all(|linux| controls.signal(linux).is_ok())
                    }
                    _ => shards_ipc::linux_signal(sig).is_some_and(|linux| controls.signal(linux).is_ok()),
                });
                drop(steered);
                let ends = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&sig);
                if !sent && ends && !ignored.contains(&sig) {
                    remove_unfinished();
                    crate::cli::terminal::restore();
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

/// A context's name: a letter or digit, then letters, digits, `_`, `.`, `+` or `-`; and not
/// `default`, this machine's. The names a `docker context` takes (v29.8.1 cli/context/store,
/// ValidateContextName), so that one named for it works here.
fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && name.len() > 1
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// The context this machine's own shards is.
pub(crate) const DEFAULT: &str = "default";

/// A context: another machine's daemon, its address and its key's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Context {
    pub name: String,
    pub description: String,
    pub address: SocketAddr,
    pub daemon: identity::Fingerprint,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn at(path: &Path) -> impl Fn(io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

/// `bytes` written to `path`, a new file readable by this user alone; an error where a file
/// is there already.
/// `bytes` at `path`, which must not exist yet: written whole under a name no record has,
/// then linked there, so that a reader (this home's daemon, woken as a name comes) finds
/// the record whole or not at all, and of two made at once, one is made and the other
/// refused.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let unnamed = unnamed(path);
    let _ = fs::remove_file(&unnamed);
    let made = write_file(&unnamed, bytes).and_then(|()| fs::hard_link(&unnamed, path));
    let _ = fs::remove_file(&unnamed);
    made.map_err(at(path))
}

/// `bytes` in place of what `path` holds, whole: written beside it, then renamed over it.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let unnamed = unnamed(path);
    let _ = fs::remove_file(&unnamed);
    let replaced = write_file(&unnamed, bytes).and_then(|()| fs::rename(&unnamed, path));
    if replaced.is_err() {
        let _ = fs::remove_file(&unnamed);
    }
    replaced.map_err(at(path))
}

/// Where `path`'s record is written before it takes its name: beside it, this process's,
/// under a name no record has, as no record's name holds `~`.
fn unnamed(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("~{name}.{}", std::process::id()))
}

/// A new file at `path`, only its owner's, holding `bytes` on disk.
fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn dir(home: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = home.join(name);
    shards_vmm::platform::create_private_dir(&dir).map_err(at(&dir))?;
    Ok(dir)
}

/// The home's identity, made the first time one is needed.
pub(crate) fn identity(home: &Path) -> Result<Identity, String> {
    shards_vmm::platform::create_private_dir(home).map_err(at(home))?;
    Identity::load_or_create(&home.join("identity")).map_err(|e| e.to_string())
}

/// The fields of a record's lines `key value`, after its first line, which must be `header`.
fn fields(text: &str, header: &str) -> Option<Vec<(String, String)>> {
    let mut lines = text.lines();
    (lines.next()? == header).then(|| {
        lines
            .filter_map(|l| l.split_once(' '))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    })
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(bytes.get(i * 2..i * 2 + 2)?).ok()?;
        *b = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

const CONTEXT: &str = "shards context 1";

impl Context {
    fn text(&self) -> String {
        format!(
            "{CONTEXT}\nname {}\naddress {}\ndaemon {}\ndescription {}\n",
            self.name,
            self.address,
            identity::hex(&self.daemon),
            self.description.replace('\n', " ")
        )
    }

    fn parse(text: &str) -> Option<Context> {
        let fields = fields(text, CONTEXT)?;
        Some(Context {
            name: field(&fields, "name")?.to_string(),
            description: field(&fields, "description").unwrap_or_default().to_string(),
            address: field(&fields, "address")?.parse().ok()?,
            daemon: unhex32(field(&fields, "daemon")?)?,
        })
    }

    /// Where its daemon is.
    pub(crate) fn remote(&self) -> Remote {
        Remote {
            address: self.address,
            daemon: self.daemon,
        }
    }
}

/// The context `name`, if this home joined one by that name.
pub(crate) fn load(home: &Path, name: &str) -> Result<Option<Context>, String> {
    if !valid_name(name) {
        return Ok(None);
    }
    let path = home.join("contexts").join(name);
    match fs::read_to_string(&path) {
        Ok(text) => Context::parse(&text)
            .map(Some)
            .ok_or_else(|| format!("{}: not a context's record", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at(&path)(e)),
    }
}

/// Every context this home joined, by name.
pub(crate) fn list(home: &Path) -> Result<Vec<Context>, String> {
    let dir = home.join("contexts");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(at(&dir)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let name = entry.map_err(at(&dir))?.file_name();
        if let Some(name) = name.to_str()
            && let Some(context) = load(home, name)?
        {
            out.push(context);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The context the home's commands use, `default` where none is chosen.
fn current(home: &Path) -> String {
    fs::read_to_string(home.join("context"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT.to_string())
}

/// What runs a command: this machine, or a context's.
pub(crate) enum Chosen {
    Here,
    There(Context),
}

/// The context `args`' command runs in, its own options taken from the front of `args`:
/// `--context NAME` (`-c NAME`, `--context=NAME`), else `SHARDS_CONTEXT`, else
/// `DOCKER_CONTEXT`, else the home's current one, so that a script's choice holds (the
/// order of docker/cli v29.8.1 cli/command/cli.go, resolveContextName); `default` is this
/// machine.
pub(crate) fn chosen(home: &Path, args: &mut Vec<OsString>) -> Result<Chosen, String> {
    let mut given = None;
    loop {
        let first = args.first().and_then(|a| a.to_str()).map(str::to_string);
        match first.as_deref() {
            Some("--context" | "-c") => {
                let name = args
                    .get(1)
                    .and_then(|a| a.to_str())
                    .ok_or("flag needs an argument: --context")?
                    .to_string();
                args.drain(..2);
                given = Some(name);
            }
            Some(flag) if flag.starts_with("--context=") => {
                given = Some(flag.trim_start_matches("--context=").to_string());
                args.remove(0);
            }
            _ => break,
        }
    }
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let name = match given {
        Some(name) => name,
        None => match env("SHARDS_CONTEXT").or_else(|| env("DOCKER_CONTEXT")) {
            Some(name) => name,
            None => current(home),
        },
    };
    if name == DEFAULT {
        return Ok(Chosen::Here);
    }
    match load(home, &name)? {
        Some(context) => Ok(Chosen::There(context)),
        None => Err(format!("context {name:?} does not exist")),
    }
}

/// What a command needs that a context does not carry yet: files of the client's that
/// Docker's CLI sends where shards would read the other machine's. The command is read as
/// shards reads it, its own grammar said as the command it runs, and its flags by its
/// parser.
fn not_carried(args: &[OsString]) -> Option<String> {
    let words: Vec<String> = args
        .iter()
        .map_while(|a| a.to_str().map(str::to_string))
        .collect();
    let said = shards_cmdline::grammar::rewrite(&words).unwrap_or_else(|| words.clone());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let first = refs.first().copied().unwrap_or_default();
    // `cp` is driven here, step by step, where this client's CLI drives one (cli/cp.rs, Unix);
    // a build's context is kept in step on the machine ([`building`]), and all else a build
    // names of this client's crosses with it ([`build_there`]).
    if shards_cmdline::commands::build(&refs).is_some() {
        return None;
    }
    // `buildx policy eval` and `test` read a policy of this client's files here, against
    // the machine as its builder: not carried yet. Every other
    // `buildx` and `builder` command asks the machine's builder alone, and `compose`,
    // which is no command of shards', is the machine's to say so of, as this one's would.
    let policy = matches!(refs.as_slice(), ["buildx", "policy", "eval" | "test", ..]);
    if policy || (first == "cp" && !cfg!(unix)) {
        let named = if policy {
            refs.get(..3).unwrap_or_default().join(" ")
        } else {
            first.to_string()
        };
        return Some(format!(
            "{named} over a context reads this machine's files, which shards does not carry yet: run it on the machine itself"
        ));
    }
    None
}

/// A build of a context of this client's (D180 part 3c): what the machine builds, of its copy
/// of the context, and what keeps the copy in step.
struct Built {
    /// The words the machine runs, its copies named by `synced::CONTEXT` and
    /// `synced::DOCKERFILE`.
    words: Vec<String>,
    /// The context here, and what a build of it excludes.
    context: PathBuf,
    filters: shards_build::context::Filters,
    /// The build file's directory's files, which go whole.
    carried: Vec<crate::synced::Carried>,
    /// The copy's name: the context's path, hashed; the build file's directory goes whole
    /// each time, whichever it is.
    key: String,
    /// The registries its bases are in, whose logins go with it.
    domains: std::collections::BTreeSet<String>,
    /// Its build file's directory here, which the machine's copy of it is said as.
    dockerfile_dir: Option<PathBuf>,
    /// Its `--secret`s as given, read here as the build is sent ([`build_there`]).
    secrets: Vec<String>,
    /// Its outputs to files or directories here: each one's name there, its path here, and
    /// whether it is a directory.
    outputs: Vec<(String, PathBuf, bool)>,
    /// The trees beside its context it names, each kept in step before it.
    trees: Vec<Tree>,
    /// Its `--ssh` specs as given, its agents this client's, reached from there.
    ssh: Vec<String>,
}

/// A tree of this client's that a build names beside its context: a named context's
/// directory, an OCI layout's, a local cache to import. Kept in step in a copy of its own
/// there before the build ([`put_tree`]), each build after sending what changed alone, and
/// said in the build's words as that copy (`synced::TREE`).
struct Tree {
    key: String,
    root: PathBuf,
    filters: shards_build::context::Filters,
}

/// The tree at `path` here, named `what` (its copy apart from any other's of the same
/// directory), its ignore file applied where `ignored`, as a build reads a named context.
fn tree_of(path: &str, what: &str, ignored: bool) -> Result<Tree, String> {
    use sha2::Digest as _;
    let root = std::path::absolute(path).map_err(|e| format!("{path}: {e}"))?;
    let exclude = if ignored {
        crate::build::ignore_file(&root, "")?
            .map(|(_, t)| shards_agentfile::ignore::read_all(&t))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut named = format!("{what}\0").into_bytes();
    named.extend_from_slice(root.as_os_str().as_encoded_bytes());
    let key = sha2::Sha256::digest(&named)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Tree {
        key,
        root,
        filters: shards_build::context::Filters {
            exclude,
            ..shards_build::context::Filters::default()
        },
    })
}

/// A `--build-context NAME=VALUE` of a directory here, or an OCI layout's: said as its copy
/// there. None for an image's, a target's, a remote one's, or a path that is no directory
/// here, which the build says so of.
fn named_here(value: &str, trees: &mut Vec<Tree>) -> Result<Option<String>, String> {
    let Some((name, v)) = value.split_once('=') else {
        return Ok(None);
    };
    let remote = [
        "http://",
        "https://",
        "git://",
        "github.com/",
        "git@",
        "docker-image://",
        "target:",
    ]
    .iter()
    .any(|p| v.starts_with(p));
    if remote {
        return Ok(None);
    }
    if let Some(layout) = crate::build::parse_oci_layout(v)? {
        let tree = tree_of(&layout.path, "layout", false)?;
        let mut said = format!("{name}=oci-layout://{}{}", crate::synced::TREE, tree.key);
        if !layout.tag.is_empty() {
            said.push_str(&format!(":{}", layout.tag));
        }
        if let Some(digest) = &layout.digest {
            said.push_str(&format!("@{digest}"));
        }
        trees.push(tree);
        return Ok(Some(said));
    }
    if !Path::new(v).is_dir() {
        return Ok(None);
    }
    let tree = tree_of(v, "named", true)?;
    let said = format!("{name}={}{}", crate::synced::TREE, tree.key);
    trees.push(tree);
    Ok(Some(said))
}

/// A CSV `value` (a cache's, a policy's) whose `field` names a path here, of the `kind` it
/// says where one is wanted (`type=local`): the path given to `place`, which says what it
/// is there. None for another kind, or one that names no path.
fn csv_path_here(
    value: &str,
    field: &str,
    kind: Option<&str>,
    place: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<Option<String>, String> {
    let Ok(fields) = shards_cmdline::go::csv_fields(value.as_bytes()) else {
        return Ok(None);
    };
    let fields: Vec<String> = fields
        .iter()
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    if kind.is_some_and(|kind| !fields.iter().any(|f| *f == format!("type={kind}"))) {
        return Ok(None);
    }
    let prefix = format!("{field}=");
    let mut placed = false;
    let mut said = Vec::new();
    for f in &fields {
        match f.strip_prefix(&prefix) {
            Some(here) if !placed => {
                placed = true;
                said.push(format!("{prefix}{}", place(here)?));
            }
            _ => said.push(f.clone()),
        }
    }
    Ok(placed.then(|| {
        said.iter()
            .map(|f| shards_cmdline::go::csv_field(f))
            .collect::<Vec<_>>()
            .join(",")
    }))
}

/// An `--output` of `value` to a file or directory here: said as the same output to the
/// machine's directory of this build's outputs, under its own name, recorded in `outputs`.
/// None for one of the machine's own, a registry's, or stdout's, and for one the build
/// refuses, which it says there.
fn output_here(value: &str, outputs: &mut Vec<(String, PathBuf, bool)>) -> Option<String> {
    let mut exports = shards_cmdline::buildflags::parse_exports(&[value.to_string()]).ok()?;
    let e = exports.pop().filter(|_| exports.is_empty())?;
    let file_or_dir = matches!(e.kind.as_str(), "local" | "tar" | "oci" | "docker");
    if !file_or_dir || e.dest.is_empty() || e.dest == "-" {
        return None;
    }
    let dir = e.kind == "local" || (e.kind != "tar" && e.attrs.get("tar").is_some_and(|t| t == "false"));
    let here = std::path::absolute(&e.dest).ok()?;
    let name = outputs.len().to_string();
    outputs.push((name.clone(), here, dir));
    let fields = [
        format!("type={}", e.kind),
        format!("dest={}{name}", crate::synced::OUT),
    ]
    .into_iter()
    .chain(e.attrs.iter().map(|(k, v)| format!("{k}={v}")));
    Some(
        fields
            .map(|f| shards_cmdline::go::csv_field(&f))
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// A file's permissions and time, for its copy there; a file not there none of its own.
fn stamped(path: &Path) -> (u32, (i64, u32)) {
    std::fs::metadata(path)
        .ok()
        .map(|m| {
            let at = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or((0, 0), |d| {
                    (i64::try_from(d.as_secs()).unwrap_or(0), d.subsec_nanos())
                });
            #[cfg(unix)]
            let mode = std::os::unix::fs::PermissionsExt::mode(&m.permissions());
            #[cfg(not(unix))]
            let mode = 0o644;
            (mode, at)
        })
        .unwrap_or((0o644, (0, 0)))
}

/// `words`' build of a context of this client's, where the command is one, read as `build`
/// reads it, its build file and ignore file found as the build finds them (build/mod.rs,
/// `definition`, `ignore_file`); a command shards' parser refuses, `-h`, and a context of
/// stdin's, Git's or a URL's go as they are. `stdin` gives `-f -`'s file.
fn building(words: &[String], stdin: &dyn Fn() -> Result<Vec<u8>, String>) -> Result<Option<Built>, String> {
    let said = shards_cmdline::grammar::rewrite(words).unwrap_or_else(|| words.to_vec());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let Some(n) = shards_cmdline::commands::build(&refs) else {
        return Ok(None);
    };
    let head = said.get(..n).map(<[String]>::to_vec).unwrap_or_default();
    let mut rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) = shards_cmdline::flags::parse(
        &shards_cmdline::commands::BUILD,
        "shards buildx build",
        &rest,
        &|_, value| Ok(value.to_string()),
    ) else {
        return Ok(None);
    };
    let (Some(arg), Some(&at)) = (parsed.args.first(), parsed.arg_words.first()) else {
        return Ok(None);
    };
    let remote = arg == "-"
        || arg.starts_with("http://")
        || arg.starts_with("https://")
        || !matches!(
            shards_agentfile::git::parse_git_ref(arg.as_bytes()),
            shards_agentfile::git::Parsed::NotGit
        );
    if remote {
        return Ok(None);
    }
    let context = std::path::absolute(arg).map_err(|e| format!("{arg}: {e}"))?;
    // One not there is the build's own to say so, there.
    if !context.is_dir() {
        return Ok(None);
    }
    // The build file, and the ignore file beside it, which overrides the context's own.
    let (name, text, beside, dir) = if parsed.string("file") == "-" {
        ("Dockerfile".to_string(), Some(stdin()?), None, None)
    } else {
        let (path, name) = crate::build::definition(&parsed, &context);
        let path = path.unwrap_or_else(|| context.join(&name));
        // `dockerfile` where `Dockerfile` is not there and it is, as the build looks for it;
        // neither there, the build says so of `Dockerfile`.
        let lower = path.with_file_name("dockerfile");
        let (path, name) = if name == "Dockerfile" && !path.exists() && lower.exists() {
            (lower, "dockerfile".to_string())
        } else {
            (path, name)
        };
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| context.clone());
        // A build file not there is the build's to say so, by its name.
        let text = std::fs::read(&path).ok();
        let beside = crate::build::ignore_file(&dir, &name)?;
        (name, text, beside, Some(dir))
    };
    let mut carried = Vec::new();
    if let Some(text) = &text {
        let (mode, mtime) = dir.as_ref().map_or((0o644, (0, 0)), |d| stamped(&d.join(&name)));
        carried.push(crate::synced::Carried {
            name: name.clone(),
            mode,
            mtime,
            bytes: text.clone(),
        });
    }
    if let (Some((ignore_name, ignore_text)), Some(dir)) = (&beside, &dir) {
        let (mode, mtime) = stamped(&dir.join(ignore_name));
        carried.push(crate::synced::Carried {
            name: ignore_name.clone(),
            mode,
            mtime,
            bytes: ignore_text.clone(),
        });
    }
    let ignore = match beside {
        Some(beside) => Some(beside),
        None => crate::build::ignore_file(&context, "")?,
    };
    let mut filters = shards_build::context::Filters {
        exclude: ignore
            .map(|(_, t)| shards_agentfile::ignore::read_all(&t))
            .unwrap_or_default(),
        ..shards_build::context::Filters::default()
    };
    // The machine's words: the context and the build file its copies', a bare build arg
    // this client's value, or none: the value where the build was asked for.
    let mut gone: Vec<(usize, usize)> = Vec::new();
    let mut outputs = Vec::new();
    let mut trees = Vec::new();
    let mut policies = Vec::new();
    for &(flag, word, offset) in &parsed.positions {
        let value = rest
            .get(word)
            .and_then(|w| w.get(offset..))
            .unwrap_or_default()
            .to_string();
        let said = match flag {
            "output" => output_here(&value, &mut outputs),
            "iidfile" | "metadata-file" if !value.is_empty() => {
                let name = if flag == "iidfile" { "iidfile" } else { "metadata" };
                let here = std::path::absolute(&value).map_err(|e| format!("{value}: {e}"))?;
                outputs.push((name.to_string(), here, false));
                Some(format!("{}{name}", crate::synced::OUT))
            }
            "build-context" => named_here(&value, &mut trees)?,
            "cache-from" => csv_path_here(&value, "src", Some("local"), &mut |here| {
                // One not here is the build's to say so, by the path it was given.
                if !Path::new(here).is_dir() {
                    return Ok(here.to_string());
                }
                let tree = tree_of(here, "cache", false)?;
                let said = format!("{}{}", crate::synced::TREE, tree.key);
                trees.push(tree);
                Ok(said)
            })?,
            "cache-to" => csv_path_here(&value, "dest", Some("local"), &mut |here| {
                let name = format!("cache-{}", outputs.len());
                let here = std::path::absolute(here).map_err(|e| format!("{here}: {e}"))?;
                outputs.push((name.clone(), here, true));
                Ok(format!("{}{name}", crate::synced::OUT))
            })?,
            // A policy's file, carried with the build file's directory, as a name of its own.
            // A policy's file, read as the build reads one, within its context (buildx's
            // os.Root of it): a name inside it said as it is there, of the copy, which holds
            // it whatever the ignore file leaves out, as the build reads the directory; one
            // outside it as it was given, which the build refuses as it would here.
            "policy" => csv_path_here(&value, "filename", None, &mut |here| {
                let path = Path::new(here);
                let inside = if path.is_absolute() {
                    path.strip_prefix(&context)
                        .ok()
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                } else {
                    Some(here.to_string())
                };
                Ok(match inside {
                    Some(inside) => {
                        policies.push(format!("!{inside}"));
                        inside
                    }
                    None => here.to_string(),
                })
            })?,
            _ => None,
        };
        if let (Some(said), Some(w)) = (said, rest.get_mut(word)) {
            *w = format!("{}{said}", w.get(..offset).unwrap_or_default());
            continue;
        }
        match flag {
            "file" | "secret" | "ssh" => gone.push((word, offset)),
            "build-arg" => {
                let value = rest
                    .get(word)
                    .and_then(|w| w.get(offset..))
                    .unwrap_or_default()
                    .to_string();
                if value.contains('=') {
                    continue;
                }
                match std::env::var(&value) {
                    Ok(here) => {
                        if let Some(w) = rest.get_mut(word) {
                            *w = format!("{}{value}={here}", w.get(..offset).unwrap_or_default());
                        }
                    }
                    Err(_) => gone.push((word, offset)),
                }
            }
            _ => {}
        }
    }
    filters
        .exclude
        .extend(policies.into_iter().map(String::into_bytes));
    if let Some(word) = rest.get_mut(at) {
        *word = crate::synced::CONTEXT.to_string();
    }
    gone.sort_unstable();
    for (word, offset) in gone.into_iter().rev() {
        without_value(&mut rest, word, offset);
    }
    let words: Vec<String> = head
        .into_iter()
        .chain(std::iter::once(format!(
            "--file={}{name}",
            crate::synced::DOCKERFILE
        )))
        .chain(rest)
        .collect();
    let key = {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(context.as_os_str().as_encoded_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    };
    let domains = text
        .as_ref()
        .map(|text| bases_of(text, &name, &parsed))
        .unwrap_or_default();
    Ok(Some(Built {
        words,
        context,
        filters,
        carried,
        key,
        domains,
        dockerfile_dir: dir,
        secrets: parsed.many("secret").to_vec(),
        outputs,
        trees,
        ssh: parsed.many("ssh").to_vec(),
    }))
}

/// An artifact's build of a directory here (`build agent DIR`, and a harness's or an MCP
/// server's): the machine's of its copy of the directory, kept in step as a build's context
/// is, all of it, as the artifact takes all it holds. A command the build refuses is its own
/// to say so, there.
fn artifact_building(words: &[String]) -> Option<Built> {
    let said = shards_cmdline::grammar::rewrite(words).unwrap_or_else(|| words.to_vec());
    let [kind_word, action, rest @ ..] = said.as_slice() else {
        return None;
    };
    let kind = crate::agent::kind_of(kind_word).filter(|_| action == "build")?;
    let args = crate::agent::build_args(kind, rest).ok()?;
    let dir = std::path::absolute(&args.dir).ok().filter(|d| d.is_dir())?;
    let mut rest = rest.to_vec();
    if let Some(word) = rest.get_mut(args.dir_at) {
        *word = crate::synced::CONTEXT.to_string();
    }
    let words = [kind_word.clone(), action.clone()]
        .into_iter()
        .chain(rest)
        .collect();
    // A copy of its own, apart from an image build's of the same directory, whose copy
    // holds what that build's ignore file leaves.
    let key = {
        use sha2::Digest as _;
        let mut named = format!("{kind_word}\0").into_bytes();
        named.extend_from_slice(dir.as_os_str().as_encoded_bytes());
        let digest = sha2::Sha256::digest(&named);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    };
    // The SBOM's scanner, which the machine pulls with this client's login, as a base.
    let domains = args
        .sbom
        .iter()
        .filter_map(|sbom| {
            let fields = shards_cmdline::go::csv_fields(sbom.as_bytes()).ok()?;
            let generator = fields
                .iter()
                .find_map(|f| {
                    String::from_utf8_lossy(f)
                        .strip_prefix("generator=")
                        .map(str::to_string)
                })
                .unwrap_or_else(|| crate::build::sbom::DEFAULT_GENERATOR.to_string());
            shards_image::reference::Reference::parse(&generator)
                .ok()
                .map(|r| r.domain)
        })
        .collect();
    Some(Built {
        words,
        context: dir,
        filters: shards_build::context::Filters::default(),
        carried: Vec::new(),
        key,
        domains,
        dockerfile_dir: None,
        secrets: Vec::new(),
        outputs: Vec::new(),
        trees: Vec::new(),
        ssh: Vec::new(),
    })
}

/// An artifact's signature through a context (`sign agent NAME`): what it is of asked of
/// the machine, whose store holds it; signed here, by this client's key or identity; and
/// kept there as its referrer, once the machine finds that it signs what the name names
/// still. `None` where the command is no artifact's signature, or asks for its help, which
/// the machine says.
fn artifact_sign_there(identity: &Identity, context: &Context, words: &[String]) -> Option<ExitCode> {
    let said = shards_cmdline::grammar::rewrite(words).unwrap_or_else(|| words.to_vec());
    let [kind_word, action, rest @ ..] = said.as_slice() else {
        return None;
    };
    let kind = crate::agent::kind_of(kind_word).filter(|_| action == "sign")?;
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        return None;
    }
    let fail = |e: String| {
        let _ = writeln!(io::stderr(), "ERROR: {e}");
        Some(ExitCode::FAILURE)
    };
    let args = match crate::agent::sign_args(kind, rest) {
        Ok(args) => args,
        Err(e) => return fail(e),
    };
    let name = args.name.to_string();
    let step = crate::agent::SIGN_STEP;
    let subject = match asked_there(identity, context, &[step, "subject", kind_word, &name], &[]) {
        Ok(said) => match serde_json::from_slice::<shards_image::oci::Descriptor>(&said) {
            Ok(subject) => subject,
            Err(e) => return fail(format!("{}: what {name} is: {e}", context.name)),
        },
        Err(code) => return Some(code),
    };
    let (bundle, who) = match crate::agent::signed(&subject, &args) {
        Ok(signed) => signed,
        Err(e) => return fail(e),
    };
    let keep = [
        step,
        "keep",
        kind_word.as_str(),
        name.as_str(),
        subject.digest.as_str(),
    ];
    let referrer = match asked_there(identity, context, &keep, &bundle) {
        Ok(said) => String::from_utf8_lossy(&said).trim().to_string(),
        Err(code) => return Some(code),
    };
    let _ = writeln!(
        io::stdout(),
        "{name}: {} signed by {who}: signature {referrer}",
        subject.digest
    );
    Some(ExitCode::SUCCESS)
}

/// What a step there says on its stdout, given `input`, its stderr said here as it comes:
/// the status this client ends with where it fails, or the machine refuses it, said so.
fn asked_there(
    identity: &Identity,
    context: &Context,
    args: &[&str],
    input: &[u8],
) -> Result<Vec<u8>, ExitCode> {
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let request = Request {
        args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
        env: Vec::new(),
        terminal: None,
    };
    let ran = client::exec(
        identity,
        context.remote(),
        request,
        Some(Box::new(io::Cursor::new(input.to_vec()))),
        Taken(out_tx),
        io::stderr(),
        |_| {},
    );
    let refused = |why: String| {
        let _ = writeln!(io::stderr(), "shards: {}: {why}", context.name);
        ExitCode::from(NOT_RUN)
    };
    match ran {
        Ok(Ok(0)) => Ok(out_rx.try_iter().flatten().collect()),
        Ok(Ok(status)) => Err(ExitCode::from(u8::try_from(status).unwrap_or(1))),
        Ok(Err(why)) => Err(refused(why)),
        Err(e) => Err(refused(e.to_string())),
    }
}

/// What a plan of a build file asks of its bases, recorded: each image answered empty.
struct Asked(std::cell::RefCell<std::collections::BTreeSet<Vec<u8>>>);

impl shards_agentfile::plan::Resolver for Asked {
    fn resolve(
        &self,
        name: &[u8],
        _platform: Option<&shards_agentfile::platform::Platform>,
        _log: &[u8],
    ) -> Result<shards_agentfile::plan::Resolved, Vec<u8>> {
        self.0.borrow_mut().insert(name.to_vec());
        Ok(shards_agentfile::plan::Resolved {
            reference: name.to_vec(),
            digest: None,
            config: b"{}".to_vec(),
        })
    }

    fn epoch(
        &self,
        _: &shards_agentfile::plan::EpochSource,
    ) -> Result<Option<shards_agentfile::plan::Epoch>, Vec<u8>> {
        Ok(None)
    }
}

/// The registries the build of `text` reaches for its bases and the images it copies from,
/// as its plan asks for them, named with its build args, target and platform: what a plan
/// whose images are answered empty asks of them. What the plan fails of, the build there
/// says.
fn bases_of(
    text: &[u8],
    name: &str,
    parsed: &shards_cmdline::flags::Parsed,
) -> std::collections::BTreeSet<String> {
    use shards_agentfile::plan;
    let host = crate::build::host_platform();
    let given = if parsed.changed("platform") {
        parsed.many("platform").to_vec()
    } else {
        crate::cli::env_ours_first("SHARDS_DEFAULT_PLATFORM", "DOCKER_DEFAULT_PLATFORM")
            .into_iter()
            .collect()
    };
    let target = crate::build::target_platforms(&given, &host)
        .ok()
        .and_then(|p| p.into_iter().next())
        .unwrap_or_else(|| host.clone());
    let opts = plan::Options {
        target_platform: target,
        build_platforms: vec![host],
        build_args: crate::build::build_args(parsed.many("build-arg"), true),
        target: parsed.string("target").as_bytes().to_vec(),
        dialect: crate::build::dialect_of(name),
        ..plan::Options::default()
    };
    let asked = Asked(std::cell::RefCell::new(std::collections::BTreeSet::new()));
    let _ = plan::plan(text, &opts, &asked);
    asked
        .0
        .into_inner()
        .into_iter()
        .filter_map(|n| shards_image::reference::Reference::parse(&String::from_utf8_lossy(&n)).ok())
        .map(|r| r.domain)
        .collect()
}

/// Builds `built` on `context`'s machine (D180 part 3c): what its copy of the context holds
/// asked, what changed here since sent as the build's stdin, with the build file's
/// directory, and the build run there of the copy, its output and status this process's.
fn build_there(
    identity: &Identity,
    context: &Context,
    built: Built,
    mut env: Vec<(String, Vec<u8>)>,
) -> ExitCode {
    let fail = |why: String| {
        let _ = writeln!(io::stderr(), "shards: {why}");
        ExitCode::from(NOT_RUN)
    };
    // The bases' registries' logins of this client's, as a pull's go with it.
    let mut found = Vec::new();
    for domain in &built.domains {
        match shards_registry::credentials::lookup(domain, &|name| std::env::var(name).ok()) {
            Ok((credentials, warnings)) => {
                for warning in warnings {
                    let _ = writeln!(io::stderr(), "{warning}");
                }
                if credentials != shards_registry::auth::Credentials::Anonymous {
                    found.push((domain.clone(), credentials));
                }
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if !found.is_empty() {
        env.retain(|(k, _)| k != "SHARDS_AUTH_CONFIG");
        env.push((
            "SHARDS_AUTH_CONFIG".to_string(),
            shards_registry::credentials::env_config_of(&found).into_bytes(),
        ));
    }
    // The other trees it names, each brought in step there first.
    for tree in &built.trees {
        if let Err(code) = put_tree(identity, context, tree) {
            return code;
        }
    }
    let step = |words: &[&str]| -> Vec<Vec<u8>> {
        let (first, rest) = words.split_first().map_or(("", &[][..]), |(f, r)| (*f, r));
        [crate::synced::SYNC_STEP, first, built.key.as_str()]
            .into_iter()
            .chain(rest.iter().copied())
            .map(|w| w.as_bytes().to_vec())
            .collect()
    };
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let request = Request {
        args: step(&["manifest"]),
        env: Vec::new(),
        terminal: None,
    };
    let asked = client::exec(
        identity,
        context.remote(),
        request,
        Some(Box::new(io::empty())),
        Taken(out_tx),
        io::stderr(),
        |_| {},
    );
    match asked {
        Ok(Ok(0)) => {}
        Ok(Ok(status)) => return ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Ok(Err(refused)) => return fail(format!("{}: {refused}", context.name)),
        Err(e) => return fail(format!("{}: {e}", context.name)),
    }
    let there = match crate::synced::decode(&out_rx.try_iter().flatten().collect::<Vec<u8>>()) {
        Ok(there) => there,
        Err(e) => return fail(e),
    };
    let walked = match shards_build::context::walk(&built.context, &built.filters) {
        Ok(walked) => walked,
        Err(e) => return fail(format!("{}: {e}", built.context.display())),
    };
    let crate::synced::Plan {
        changed,
        removed,
        linked,
    } = crate::synced::plan(&walked, &there);
    // Its secrets, read here as a build here reads them, the last of an id counting: each
    // sent with it, and read there from a pipe of its own, which no file of the machine's
    // holds; said by its id and its pipe, before the context.
    let here = |name: &str| std::env::var_os(name).map(crate::build::os_bytes);
    let secrets = match shards_cmdline::buildflags::parse_secrets(&built.secrets)
        .and_then(|s| shards_cmdline::buildflags::store(s, &here, u64::from(shards_abi::run::MAX_PAYLOAD)))
    {
        Ok(store) => store.into_iter().collect::<Vec<_>>(),
        Err(e) => {
            let _ = writeln!(io::stderr(), "ERROR: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Its SSH agents, resolved here as a build here resolves them: each a socket there whose
    // connections come back to this one, an agent's socket or the keys of files.
    let mut agents: crate::build::Agents = match shards_cmdline::buildflags::ssh_agents(
        &shards_cmdline::buildflags::parse_ssh(&built.ssh),
        &|k| std::env::var(k).ok(),
        &crate::build::sshkey::parse,
    ) {
        Ok(agents) => agents,
        Err(e) => {
            let _ = writeln!(io::stderr(), "ERROR: {e}");
            return ExitCode::FAILURE;
        }
    };
    for agent in agents.values_mut() {
        if let shards_cmdline::buildflags::Agent::Keys(keys) = agent {
            *keys = crate::build::sshkey::keyring(std::mem::take(keys));
        }
    }
    let mut words = built.words.clone();
    let at = words
        .iter()
        .position(|w| w == crate::synced::CONTEXT)
        .unwrap_or(words.len());
    for id in agents.keys().rev() {
        words.insert(at, format!("--ssh={id}={}{id}", crate::synced::AGENT_SOCKET));
    }
    for (i, (id, _)) in secrets.iter().enumerate().rev() {
        let id = shards_cmdline::go::csv_field(&format!("id={id}"));
        words.insert(at, format!("--secret={id},src={}{i}", crate::synced::SECRET));
    }
    // This client's own paths, which what the build prints there says in place of its
    // copies': its context's, its build file's directory's, and each tree's, by its key.
    let mut shown = built.context.as_os_str().as_encoded_bytes().to_vec();
    shown.push(0);
    if let Some(dir) = &built.dockerfile_dir {
        shown.extend_from_slice(dir.as_os_str().as_encoded_bytes());
    }
    for tree in &built.trees {
        shown.push(0);
        shown.extend_from_slice(tree.key.as_bytes());
        shown.push(0);
        shown.extend_from_slice(tree.root.as_os_str().as_encoded_bytes());
    }
    env.push((crate::synced::SHOWN_AS.to_string(), shown));
    let (reader, mut writer) = match io::pipe() {
        Ok(pipe) => pipe,
        Err(e) => return fail(format!("a pipe for the context: {e}")),
    };
    let (root, carried) = (built.context.clone(), built.carried);
    let sending = std::thread::Builder::new()
        .name("context-send".into())
        .spawn(move || {
            crate::synced::send(&mut writer, &carried, &secrets, |out| {
                shards_archive::pack_changes(&root, &changed, &linked, &removed, out)
                    .map(drop)
                    .map_err(|e| e.to_string())
            })
        });
    let sending = match sending {
        Ok(sending) => sending,
        Err(e) => return fail(format!("a thread for the context: {e}")),
    };
    let words: Vec<&str> = std::iter::once("build")
        .chain(words.iter().map(String::as_str))
        .collect();
    // This client's terminal, where its stderr is one: the build there draws shards' own
    // display on a terminal of this one's size, as a build here draws it (build/live.rs);
    // its stdin and stdout are the step's streams.
    let terminal = here_terminal().filter(|t| t.stderr).map(|t| frame::Terminal {
        stdin: false,
        stdout: false,
        ..t
    });
    let request = Request {
        args: step(&words),
        env,
        terminal,
    };
    // A resize since the size was said is sent once the build is asked for, as no signal
    // could send it before.
    let steered = move |controls: client::Controls| {
        #[cfg(unix)]
        if let Some(t) = terminal {
            let now = crate::cli::terminal::size(2);
            if now != (t.rows, t.cols) {
                let _ = controls.resize(now.0, now.1);
            }
        }
        *STEERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(controls);
    };
    // What the build says on stdout, its own and then its outputs of this client's, taken
    // apart as it comes, each output kept here as it is said; and each connection a step
    // makes to an agent, relayed to this client's agent of its id, as it is made.
    let (failed_tx, failed_rx) = std::sync::mpsc::channel();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<(String, String)>();
    let fetched = Fetched {
        wanted: built.outputs,
        current: None,
        failed: failed_tx,
        agents: agent_tx,
    };
    let ran = std::thread::scope(|scope| {
        let (agents, key) = (&agents, built.key.as_str());
        std::thread::Builder::new()
            .name("build-agents".into())
            .spawn_scoped(scope, move || {
                for (id, token) in agent_rx {
                    let Some(agent) = agents.get(&id) else {
                        continue;
                    };
                    let relayed = std::thread::Builder::new()
                        .name("build-agent".into())
                        .spawn_scoped(scope, move || relay_agent(identity, context, key, &token, agent));
                    if let Err(e) = relayed {
                        let _ = writeln!(io::stderr(), "shards: a thread for an agent: {e}");
                    }
                }
            })
            .map_err(|e| shards_remote::Error::new(format!("a thread for the build's agents: {e}")))?;
        client::exec(
            identity,
            context.remote(),
            request,
            Some(Box::new(reader)),
            crate::synced::Demux::new(io::stdout(), fetched),
            io::stderr(),
            steered,
        )
    });
    let sent = sending.join();
    // An output not kept is the build's failure, as one the build here could not write is.
    if let Ok(why) = failed_rx.try_recv() {
        let _ = writeln!(io::stderr(), "ERROR: {why}");
        return ExitCode::FAILURE;
    }
    match ran {
        Ok(Ok(status)) => {
            if status == 0
                && let Ok(Err(e)) = sent
            {
                return fail(e);
            }
            ExitCode::from(u8::try_from(status).unwrap_or(1))
        }
        Ok(Err(refused)) => fail(format!("{}: {refused}", context.name)),
        Err(e) => fail(format!("{}: {e}", context.name)),
    }
}

/// Brings `tree`'s copy there in step with it here, before the build that names it: what its
/// copy holds asked, and what changed since sent alone, as a build's context is (part 3c).
fn put_tree(identity: &Identity, context: &Context, tree: &Tree) -> Result<(), ExitCode> {
    let fail = |why: String| {
        let _ = writeln!(io::stderr(), "shards: {why}");
        ExitCode::from(NOT_RUN)
    };
    let step = crate::synced::SYNC_STEP;
    let said = asked_there(identity, context, &[step, "manifest", &tree.key], &[])?;
    let there = crate::synced::decode(&said).map_err(fail)?;
    let walked = shards_build::context::walk(&tree.root, &tree.filters)
        .map_err(|e| fail(format!("{}: {e}", tree.root.display())))?;
    let plan = crate::synced::plan(&walked, &there);
    let (reader, mut writer) = io::pipe().map_err(|e| fail(format!("a pipe for a tree: {e}")))?;
    let root = tree.root.clone();
    let sending = std::thread::Builder::new()
        .name("tree-send".into())
        .spawn(move || {
            crate::synced::send_tree(&mut writer, |out| {
                shards_archive::pack_changes(&root, &plan.changed, &plan.linked, &plan.removed, out)
                    .map(drop)
                    .map_err(|e| e.to_string())
            })
        })
        .map_err(|e| fail(format!("a thread for a tree: {e}")))?;
    let request = Request {
        args: [step, "put", tree.key.as_str()]
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect(),
        env: Vec::new(),
        terminal: None,
    };
    let ran = client::exec(
        identity,
        context.remote(),
        request,
        Some(Box::new(reader)),
        io::sink(),
        io::stderr(),
        |_| {},
    );
    let sent = sending.join();
    match ran {
        Ok(Ok(0)) => {}
        Ok(Ok(status)) => return Err(ExitCode::from(u8::try_from(status).unwrap_or(1))),
        Ok(Err(why)) => return Err(fail(format!("{}: {why}", context.name))),
        Err(e) => return Err(fail(format!("{}: {e}", context.name))),
    }
    match sent {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(fail(e)),
        Err(_) => Err(fail("sending a tree ended".into())),
    }
}

/// Where a build's outputs of this client's go as they come back (`synced::Outputs`): a
/// file whole, beside its path and renamed to it at its end, its mode a new file's as a
/// build here writes one; a directory's archive unpacked into it, as the local exporter
/// writes one. What fails is said on `failed`.
struct Fetched {
    wanted: Vec<(String, PathBuf, bool)>,
    current: Option<Fetching>,
    failed: std::sync::mpsc::Sender<String>,
    /// Each connection a step makes to an agent, its id and token, for [`relay_agent`].
    agents: std::sync::mpsc::Sender<(String, String)>,
}

enum Fetching {
    File {
        file: fs::File,
        temp: PathBuf,
        dest: PathBuf,
    },
    Dir {
        input: io::PipeWriter,
        unpacking: std::thread::JoinHandle<Result<(), String>>,
        dest: PathBuf,
    },
}

impl Fetched {
    /// `e` said on `failed`, at `dest`, and as the error the step's writer ends with.
    fn failed(&self, dest: &Path, e: impl std::fmt::Display) -> io::Error {
        let why = format!("{}: {e}", dest.display());
        let _ = self.failed.send(why.clone());
        io::Error::other(why)
    }
}

impl crate::synced::Outputs for Fetched {
    fn begin(&mut self, name: &str, dir: bool) -> io::Result<()> {
        let Some((_, dest, _)) = self.wanted.iter().find(|(n, ..)| n == name) else {
            return Err(io::Error::other(format!("an output {name:?} not asked for")));
        };
        let dest = dest.clone();
        if dir {
            fs::create_dir_all(&dest).map_err(|e| self.failed(&dest, e))?;
            let (output, input) = io::pipe().map_err(|e| self.failed(&dest, e))?;
            let to = dest.clone();
            let unpacking = std::thread::Builder::new()
                .name("build-output".into())
                .spawn(move || {
                    let opts = shards_archive::UnpackOptions {
                        no_lchown: true,
                        ..shards_archive::UnpackOptions::default()
                    };
                    shards_archive::untar(output, &to, &opts).map_err(|e| e.to_string())
                })
                .map_err(|e| self.failed(&dest, e))?;
            self.current = Some(Fetching::Dir {
                input,
                unpacking,
                dest,
            });
            return Ok(());
        }
        let base = dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let temp = dest.with_file_name(format!(".{base}.shards-{}", std::process::id()));
        let _ = fs::remove_file(&temp);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| self.failed(&dest, e))?;
        *UNFINISHED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(temp.clone());
        self.current = Some(Fetching::File { file, temp, dest });
        Ok(())
    }

    fn chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        let written = match &mut self.current {
            Some(Fetching::File { file, dest, .. }) => file.write_all(bytes).map_err(|e| (dest.clone(), e)),
            Some(Fetching::Dir { input, dest, .. }) => input.write_all(bytes).map_err(|e| (dest.clone(), e)),
            None => Ok(()),
        };
        written.map_err(|(dest, e)| self.failed(&dest, e))
    }

    fn end(&mut self) -> io::Result<()> {
        match self.current.take() {
            Some(Fetching::File { file, temp, dest }) => {
                let kept = file.sync_all().and_then(|()| fs::rename(&temp, &dest));
                *UNFINISHED
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                if let Err(e) = kept {
                    let _ = fs::remove_file(&temp);
                    return Err(self.failed(&dest, e));
                }
                Ok(())
            }
            Some(Fetching::Dir {
                input,
                unpacking,
                dest,
            }) => {
                drop(input);
                match unpacking.join() {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(self.failed(&dest, e)),
                    Err(_) => Err(self.failed(&dest, "unpacking it ended")),
                }
            }
            None => Ok(()),
        }
    }

    fn agent(&mut self, id: &str, token: &str) -> io::Result<()> {
        let _ = self.agents.send((id.to_string(), token.to_string()));
        Ok(())
    }
}

/// Relays a step's connection to an SSH agent, claimed by `token` from the machine, to this
/// client's `agent`: each request the step makes answered by its socket, or by its keys as
/// BuildKit's keyring answers; and only what a read-only agent answers (sshprovider
/// readOnlyAgent), refused here too, whatever the machine let by, so that no machine has
/// this client's agent forget or take a key.
fn relay_agent(
    identity: &Identity,
    context: &Context,
    key: &str,
    token: &str,
    agent: &shards_cmdline::buildflags::Agent<crate::build::sshkey::Key>,
) {
    let (Ok((asked, asking)), Ok((answers, answering))) = (io::pipe(), io::pipe()) else {
        return;
    };
    let request = Request {
        args: [crate::synced::SYNC_STEP, "agent", key, token]
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect(),
        env: Vec::new(),
        terminal: None,
    };
    std::thread::scope(|scope| {
        let answered = std::thread::Builder::new()
            .name("build-agent-answers".into())
            .spawn_scoped(scope, move || answer_agent(asked, answering, agent));
        let _ = client::exec(
            identity,
            context.remote(),
            request,
            Some(Box::new(answers)),
            asking,
            io::sink(),
            |_| {},
        );
        if let Ok(answered) = answered {
            let _ = answered.join();
        }
    });
}

/// Answers the agent requests `asked` holds, on `answering`, as [`relay_agent`] says.
fn answer_agent(
    mut asked: io::PipeReader,
    mut answering: io::PipeWriter,
    agent: &shards_cmdline::buildflags::Agent<crate::build::sshkey::Key>,
) -> io::Result<()> {
    let most = crate::build::builder::AGENT_MAX;
    let read_msg = |from: &mut dyn io::Read| -> io::Result<Option<Vec<u8>>> {
        let mut n = [0u8; 4];
        match from.read_exact(&mut n) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = usize::try_from(u32::from_be_bytes(n)).unwrap_or(usize::MAX);
        if n == 0 || n > most {
            return Err(io::Error::other("an agent message of a bad length"));
        }
        let mut body = vec![0u8; n];
        from.read_exact(&mut body)?;
        Ok(Some(body))
    };
    let write_msg = |to: &mut dyn io::Write, body: &[u8]| -> io::Result<()> {
        to.write_all(&u32::try_from(body.len()).unwrap_or(0).to_be_bytes())?;
        to.write_all(body)
    };
    // SSH_AGENT_FAILURE (draft-ietf-sshm-ssh-agent §5.1).
    let refused = [5u8];
    let mut socket = None;
    while let Some(request) = read_msg(&mut asked)? {
        let allowed = request
            .first()
            .is_some_and(|t| crate::build::builder::AGENT_ALLOWED.contains(t));
        let answer = match agent {
            _ if !allowed => refused.to_vec(),
            shards_cmdline::buildflags::Agent::Keys(keys) => crate::build::sshkey::answer(keys, &request),
            shards_cmdline::buildflags::Agent::Socket(path) => {
                let stream = match &mut socket {
                    Some(stream) => stream,
                    None => socket.insert(crate::build::ssh::agent_stream(path)?),
                };
                write_msg(&mut **stream, &request)?;
                read_msg(&mut **stream)?.unwrap_or_else(|| refused.to_vec())
            }
        };
        write_msg(&mut answering, &answer)?;
    }
    Ok(())
}

impl Drop for Fetched {
    /// A file not whole goes: none is left of a build that ended before its output did.
    fn drop(&mut self) {
        if let Some(Fetching::File { temp, .. }) = self.current.take() {
            let _ = fs::remove_file(&temp);
            *UNFINISHED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }
}

/// A reader of a file of `KEY=VALUE` lines, a key alone its value here where the bool says
/// so (the CLI's kv_file, pkg/kvfile's Parse).
type KvFile = dyn Fn(&str, bool) -> Result<Vec<String>, String>;

/// What a command line names of this client's, resolved here, where the command there
/// would read its own machine's (D180): a variable `-e NAME` takes
/// the value `lookup` gives it here, if any (docker/cli opts/env.go, ValidateEnv); and
/// `--env-file` and `--label-file`, files of this machine's, become the `--env` and
/// `--label` values that `read` finds in them, ahead of the rest (the order a `docker`
/// script's values take, docker/cli ReadKVEnvStrings, ReadKVStrings); and a seccomp
/// profile's file, read here and sent as its JSON (docker/cli parseSecurityOpts sends it so
/// too), which the command there takes as the profile. A command shards' parser refuses goes as it is: its refusal is the command's
/// own there. What a file here fails of is said as the command says it there, with its exit
/// status.
fn here_values(
    words: Vec<String>,
    lookup: &dyn Fn(&str) -> Option<String>,
    read: &KvFile,
) -> Result<Vec<String>, (String, u8)> {
    use shards_cmdline::commands::{CREATE, EXEC, RUN};
    let said = shards_cmdline::grammar::rewrite(&words).unwrap_or_else(|| words.clone());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let (command, path, n) = match refs.as_slice() {
        ["run", ..] => (&RUN, "shards run", 1),
        ["container", "run", ..] => (&RUN, "shards container run", 2),
        ["create", ..] => (&CREATE, "shards create", 1),
        ["container", "create", ..] => (&CREATE, "shards container create", 2),
        ["exec", ..] => (&EXEC, "shards exec", 1),
        ["container", "exec", ..] => (&EXEC, "shards container exec", 2),
        _ => return Ok(words),
    };
    let head = said.get(..n).map(<[String]>::to_vec).unwrap_or_default();
    let mut rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) =
        shards_cmdline::flags::parse(command, path, &rest, &|_, value| Ok(value.to_string()))
    else {
        return Ok(words);
    };
    // As run and create word their objections (request.rs, docker/cli run.go withHelp), and
    // exec its own, plain.
    let refused = |e: String| {
        if std::ptr::eq(command, &EXEC) {
            (e, 1)
        } else {
            (
                format!("shards: {e}\n\nRun '{path} --help' for more information"),
                NOT_RUN,
            )
        }
    };
    let mut from_files = Vec::new();
    let mut gone = Vec::new();
    for &(name, at, offset) in &parsed.positions {
        let Some(word) = rest.get_mut(at) else {
            continue;
        };
        let value = word.get(offset..).unwrap_or_default().to_string();
        match name {
            "env" if !value.is_empty() && !value.contains('=') => {
                if let Some(here) = lookup(&value) {
                    *word = format!("{}{value}={here}", word.get(..offset).unwrap_or_default());
                }
            }
            "env-file" | "label-file" => {
                let flag = if name == "env-file" { "--env" } else { "--label" };
                for entry in read(&value, name == "env-file").map_err(refused)? {
                    from_files.push(format!("{flag}={entry}"));
                }
                gone.push(at);
                // The flag's own word, before a value of a word of its own.
                if offset == 0 {
                    gone.extend(at.checked_sub(1));
                }
            }
            "security-opt" => {
                let kv = value.split_once('=').or_else(|| value.split_once(':'));
                if let Some(("seccomp", v)) = kv
                    && !matches!(v, "" | "builtin" | "unconfined")
                {
                    let profile = crate::cli::request::seccomp_profile(v).map_err(refused)?;
                    *word = format!("{}seccomp={profile}", word.get(..offset).unwrap_or_default());
                }
            }
            _ => {}
        }
    }
    gone.sort_unstable();
    gone.dedup();
    for at in gone.into_iter().rev() {
        if at < rest.len() {
            rest.remove(at);
        }
    }
    Ok(head.into_iter().chain(from_files).chain(rest).collect())
}

/// A run's or create's `--cidfile` here, and how its command words what it refuses.
struct Cid {
    file: crate::cli::request::CidFile,
    run: bool,
}

impl Cid {
    /// `why` as the command says it, and its status: a run's with its help line, as
    /// docker/cli's runRun words what createContainer returns (cli/request.rs), and a
    /// create's plain.
    fn refused(&self, why: String) -> (String, u8) {
        refused_cid(self.run, why)
    }
}

fn refused_cid(run: bool, why: String) -> (String, u8) {
    if run {
        let status = crate::cli::request::run_status(&why);
        (
            format!("shards: {why}\n\nRun 'shards run --help' for more information"),
            status,
        )
    } else {
        (why, 1)
    }
}

/// A run's or create's `--cidfile`, a file of this client's that Docker's CLI makes and
/// writes (D180): made here, in one step that fails where one is there, before the command is asked
/// for, and said there as [`CIDFILE`] before its path here, which the run records; the
/// machine tells this client the ID once it has made the microVM, and this client writes it
/// (`client::exec_made`). A platform that does not parse is the machine's to refuse first,
/// the order a `docker` script sees (docker/cli parses one before it makes the file), and a
/// command shards' parser refuses goes as it is.
fn here_cidfile(words: Vec<String>) -> Result<(Vec<String>, Option<Cid>), (bool, String)> {
    use shards_cmdline::commands::{CREATE, RUN};
    let said = shards_cmdline::grammar::rewrite(&words).unwrap_or_else(|| words.clone());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let (command, path, n) = match refs.as_slice() {
        ["run", ..] => (&RUN, "shards run", 1),
        ["container", "run", ..] => (&RUN, "shards container run", 2),
        ["create", ..] => (&CREATE, "shards create", 1),
        ["container", "create", ..] => (&CREATE, "shards container create", 2),
        _ => return Ok((words, None)),
    };
    let head = said.get(..n).map(<[String]>::to_vec).unwrap_or_default();
    let mut rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) =
        shards_cmdline::flags::parse(command, path, &rest, &|_, value| Ok(value.to_string()))
    else {
        return Ok((words, None));
    };
    let file = parsed.string("cidfile").to_string();
    let platform = if parsed.changed("platform") {
        parsed.string("platform").to_string()
    } else {
        crate::cli::env_ours_first("SHARDS_DEFAULT_PLATFORM", "DOCKER_DEFAULT_PLATFORM").unwrap_or_default()
    };
    if file.is_empty() || (!platform.is_empty() && crate::pull::targets(&platform).is_err()) {
        return Ok((words, None));
    }
    let run = std::ptr::eq(command, &RUN);
    let here = std::path::absolute(&file)
        .map_err(|e| (run, format!("failed to create the container ID file: {e}")))?;
    let here = here.to_string_lossy().into_owned();
    let made = crate::cli::request::CidFile::new(&here).map_err(|e| (run, e))?;
    for &(name, at, offset) in &parsed.positions {
        if name != "cidfile" {
            continue;
        }
        if let Some(word) = rest.get_mut(at) {
            *word = format!("{}{CIDFILE}{here}", word.get(..offset).unwrap_or_default());
        }
    }
    Ok((
        head.into_iter().chain(rest).collect(),
        Some(Cid { file: made, run }),
    ))
}

/// A file of this client's that Docker's CLI writes or reads whole for its command (D180):
/// the command there writes it as its stdout, or reads it as its stdin, and this client
/// writes the file, or reads it.
#[derive(Debug, PartialEq)]
enum Named {
    /// What the command writes goes to the file here, kept whole or not at all, as a save
    /// here keeps it (cli/save.rs); `failed` begins what a failure says.
    Written { path: String, failed: &'static str },
    /// The file here that `then`, a command run there once the command is through, writes
    /// as [`Named::Written`]'s: the Agentfile of the image a pull pulled.
    WrittenAfter {
        path: String,
        failed: &'static str,
        then: Vec<String>,
    },
    /// The command reads the file here.
    Read(String),
}

/// The file of this client's that the command `words` writes or reads whole, if any, and
/// the command that writes its stdout or reads its stdin there instead (D180): `save -o`,
/// `export -o` and `make agentfile -o` write theirs here, `load -i` reads its file here,
/// and `import` a file it names, the client's files (the files docker/cli's runSave,
/// runExport, runLoad and runImport write and read too, cli/command/image/save.go, load.go,
/// import.go, container/export.go); and `pull --output-agentfile` writes the image's
/// Agentfile here,
/// asked of the machine once its pull is through. A command shards' parser refuses, or
/// `-h`, goes as it is.
fn here_files(words: Vec<String>) -> (Vec<String>, Option<Named>) {
    use shards_cmdline::commands::{EXPORT, IMPORT, LOAD, PULL, SAVE};
    let said = shards_cmdline::grammar::rewrite(&words).unwrap_or_else(|| words.clone());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    if refs.first() == Some(&"agentfile") {
        return agentfile_output(words, &said);
    }
    let Some((command, path, n)) = shards_cmdline::commands::find(&refs) else {
        return (words, None);
    };
    let (flag, failed) = if std::ptr::eq(command, &SAVE) {
        ("output", "failed to save image")
    } else if std::ptr::eq(command, &EXPORT) {
        ("output", "failed to export container")
    } else if std::ptr::eq(command, &LOAD) {
        ("input", "")
    } else if std::ptr::eq(command, &IMPORT) {
        ("", "")
    } else if std::ptr::eq(command, &PULL) {
        ("output-agentfile", crate::from_image::FAILED)
    } else {
        return (words, None);
    };
    let head = said.get(..n).map(<[String]>::to_vec).unwrap_or_default();
    let mut rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) =
        shards_cmdline::flags::parse(command, path, &rest, &|_, value| Ok(value.to_string()))
    else {
        return (words, None);
    };
    let named = if flag.is_empty() {
        // `-` is stdin, and a URL the daemon's to fetch.
        match (parsed.args.first(), parsed.arg_words.first()) {
            (Some(source), Some(&at))
                if source != "-" && !source.starts_with("http://") && !source.starts_with("https://") =>
            {
                let named = Named::Read(source.clone());
                if let Some(word) = rest.get_mut(at) {
                    *word = "-".into();
                }
                named
            }
            _ => return (words, None),
        }
    } else {
        let file = parsed.string(flag).to_string();
        if file.is_empty() {
            return (words, None);
        }
        let mut given: Vec<(usize, usize)> = parsed
            .positions
            .iter()
            .filter(|(name, ..)| *name == flag)
            .map(|&(_, at, offset)| (at, offset))
            .collect();
        given.sort_unstable();
        for (at, offset) in given.into_iter().rev() {
            without_value(&mut rest, at, offset);
        }
        if flag == "input" {
            Named::Read(file)
        } else if flag == "output-agentfile" {
            // `-a`, which names no one image, is refused there as here.
            if parsed.bool("all-tags") {
                return (words, None);
            }
            let mut then = vec!["agentfile".to_string()];
            if parsed.changed("platform") {
                then.extend(["--platform".to_string(), parsed.string("platform").to_string()]);
            }
            then.extend(parsed.args.first().cloned());
            Named::WrittenAfter {
                path: crate::from_image::output_path(&file),
                failed,
                then,
            }
        } else {
            Named::Written { path: file, failed }
        }
    };
    (head.into_iter().chain(rest).collect(), Some(named))
}

/// `words` without the value a flag took at word `at`, from byte `offset`, nor the flag:
/// a word of its own goes with the flag's before it, and a value in the flag's word with
/// it, but the flags it ran together with stay (`-qi FILE` is `-q`), as pflag reads them.
fn without_value(words: &mut Vec<String>, at: usize, offset: usize) {
    let (flag, alone) = if offset == 0 {
        if at < words.len() {
            words.remove(at);
        }
        (at.checked_sub(1), true)
    } else {
        (Some(at), false)
    };
    let Some(flag) = flag else {
        return;
    };
    let Some(word) = words.get_mut(flag) else {
        return;
    };
    if word.starts_with("--") {
        words.remove(flag);
        return;
    }
    // A short group: `-`, the flags before, the flag's letter, and `=` before a value in
    // the word (`-i=FILE`).
    let mut group = if alone {
        word.clone()
    } else {
        word.get(..offset).unwrap_or_default().to_string()
    };
    if !alone && group.len() > 2 && group.ends_with('=') {
        group.pop();
    }
    group.pop();
    if group.len() > 1 {
        *word = group;
    } else {
        words.remove(flag);
    }
}

/// `make agentfile -o FILE` (or `--output FILE`, `--output=FILE`), as from_image.rs reads
/// it, the last one named its file: the Agentfile written to the command's stdout there.
fn agentfile_output(words: Vec<String>, said: &[String]) -> (Vec<String>, Option<Named>) {
    let mut kept = Vec::with_capacity(said.len());
    let mut file = None;
    let mut it = said.iter();
    while let Some(word) = it.next() {
        match word.as_str() {
            "-h" | "--help" => return (words, None),
            "-o" | "--output" => match it.next() {
                Some(path) => file = Some(path.clone()),
                None => return (words, None),
            },
            w if w.starts_with("--output=") => file = Some(w.trim_start_matches("--output=").to_string()),
            _ => kept.push(word.clone()),
        }
    }
    match file {
        Some(path) if !path.is_empty() => (
            kept,
            Some(Named::Written {
                path,
                failed: "failed to write the Agentfile",
            }),
        ),
        _ => (words, None),
    }
}

/// Whether `args`, the command after shards', is a context's own, which runs on this machine
/// whatever the context: what it joined, and who joins it.
pub(crate) fn kept_here(args: &[OsString]) -> bool {
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).collect();
    let own = |w: &str| {
        matches!(
            w,
            "context" | "contexts" | "client" | "clients" | "invitation" | "invitations"
        )
    };
    // A login is the client's, as `docker --context NAME login` keeps it (docker/cli
    // cli/command/registry/login.go): checked with the registry and kept where this
    // client's commands find it, never in the machine's store.
    words.is_empty()
        || words
            .first()
            .is_some_and(|w| own(w) || matches!(*w, "help" | "-h" | "--help" | "login" | "logout"))
        || words.get(1).is_some_and(|w| own(w))
}

/// This client's credentials for the registry the command `words` reaches there, sent with
/// the command (D180): those
/// for the image a pull, push, run or create names, or the index a search asks, and of
/// shards' own, the image `make agentfile` names, which it pulls where it is not there, and
/// the artifact an agent's, harness's or MCP server's push, pull or referrer's removal
/// names; as `SHARDS_AUTH_CONFIG`'s value, none for none; and what finding them warned of.
/// What a local pull fails of, the command fails of here, before it runs.
fn own_credentials(
    words: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Option<String>, Vec<String>), String> {
    use shards_cmdline::commands::{CREATE, PULL, PUSH, RUN, SEARCH};
    use shards_registry::auth::Credentials;
    let said = shards_cmdline::grammar::rewrite(words).unwrap_or_else(|| words.to_vec());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let lookup = |domain: String| -> Result<(Option<String>, Vec<String>), String> {
        let (found, warnings) =
            shards_registry::credentials::lookup(&domain, env).map_err(|e| e.to_string())?;
        let sent = (found != Credentials::Anonymous)
            .then(|| shards_registry::credentials::env_config_of(&[(domain, found)]));
        Ok((sent, warnings))
    };
    // shards' own, which read their words themselves (from_image.rs, agent.rs).
    if let Some(named) = own_named(&refs) {
        return match shards_image::reference::Reference::parse(named) {
            Ok(reference) => lookup(reference.domain),
            Err(_) => Ok((None, Vec::new())),
        };
    }
    let (command, path, n) = match refs.as_slice() {
        ["pull", ..] => (&PULL, "shards pull", 1),
        ["image", "pull", ..] => (&PULL, "shards image pull", 2),
        ["push", ..] => (&PUSH, "shards push", 1),
        ["image", "push", ..] => (&PUSH, "shards image push", 2),
        ["run", ..] => (&RUN, "shards run", 1),
        ["container", "run", ..] => (&RUN, "shards container run", 2),
        ["create", ..] => (&CREATE, "shards create", 1),
        ["container", "create", ..] => (&CREATE, "shards container create", 2),
        ["search", ..] => (&SEARCH, "shards search", 1),
        _ => return Ok((None, Vec::new())),
    };
    let rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) =
        shards_cmdline::flags::parse(command, path, &rest, &|_, value| Ok(value.to_string()))
    else {
        return Ok((None, Vec::new()));
    };
    let Some(named) = parsed.args.first() else {
        return Ok((None, Vec::new()));
    };
    // A run that never pulls reaches no registry: no helper is asked for what would not be
    // sent, as docker/cli asks one only to pull (cli/command/container/create.go,
    // pullImage), and one that never answers holds up no such run.
    if (std::ptr::eq(command, &RUN) || std::ptr::eq(command, &CREATE)) && parsed.string("pull") == "never" {
        return Ok((None, Vec::new()));
    }
    let domain = if std::ptr::eq(command, &SEARCH) {
        crate::search::host(&shards_registry::search::searched(named).0).to_string()
    } else {
        // A name the command refuses there is its own to refuse.
        match shards_image::reference::Reference::parse(named) {
            Ok(reference) => reference.domain,
            Err(_) => return Ok((None, Vec::new())),
        }
    };
    lookup(domain)
}

/// The image or artifact one of shards' own commands names, where it reaches a registry
/// (`refs`, said as the command it runs): `make agentfile`'s image, the word that is no
/// flag nor `-o`'s or `--platform`'s value; an artifact's push and pull, the one name; and a
/// referrer's removal, the one name besides `--referrer`'s value.
fn own_named<'a>(refs: &[&'a str]) -> Option<&'a str> {
    match refs {
        ["agentfile", rest @ ..] => {
            let mut it = rest.iter();
            while let Some(word) = it.next() {
                match *word {
                    "-o" | "--output" | "--platform" => {
                        it.next();
                    }
                    w if w.starts_with('-') => {}
                    w => return Some(w),
                }
            }
            None
        }
        ["agent" | "harness" | "mcp", "push" | "pull", name] => Some(name),
        ["agent" | "harness" | "mcp", "rm" | "remove" | "delete", rest @ ..]
            if rest
                .iter()
                .any(|w| *w == "--referrer" || w.starts_with("--referrer=")) =>
        {
            let mut it = rest.iter();
            let mut names = Vec::new();
            while let Some(word) = it.next() {
                match *word {
                    "--referrer" => {
                        it.next();
                    }
                    w if w.starts_with('-') => {}
                    w => names.push(w),
                }
            }
            match names.as_slice() {
                [one] => Some(one),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether this home's daemon serves now: its request slots (D185).
fn daemon_serves(home: &Path) -> bool {
    #[cfg(unix)]
    {
        shards_ipc::slots::served(home)
    }
    #[cfg(not(unix))]
    {
        let _ = home;
        false
    }
}

/// Where this home's daemon is reached from outside shards' own CLI: its Engine API's
/// socket (D170), which Docker's clients and SDKs take as their endpoint.
fn own_endpoint(home: &Path) -> String {
    #[cfg(unix)]
    {
        format!("unix://{}", home.join(crate::daemon::ENGINE_SOCKET).display())
    }
    #[cfg(not(unix))]
    {
        home.display().to_string()
    }
}

/// Runs `args` on `context`'s machine: what it prints, here, and its exit status, this
/// process's.
pub(crate) fn forward(home: &Path, context: &Context, args: Vec<OsString>) -> ExitCode {
    let fail = |why: String| {
        let _ = writeln!(io::stderr(), "shards: {why}");
        ExitCode::from(NOT_RUN)
    };
    if let Some(why) = not_carried(&args) {
        return fail(why);
    }
    let identity = match identity(home) {
        Ok(identity) => identity,
        Err(e) => return fail(e),
    };
    let mut words = Vec::with_capacity(args.len());
    for arg in &args {
        match arg.to_str() {
            Some(text) => words.push(text.to_string()),
            None => {
                return fail(format!(
                    "{arg:?}: an argument that is not UTF-8 does not cross to another machine"
                ));
            }
        }
    }
    let words = match here_values(
        words,
        &|name| std::env::var(name).ok(),
        &crate::cli::request::kv_file,
    ) {
        Ok(words) => words,
        Err((said, status)) => {
            let _ = writeln!(io::stderr(), "{said}");
            return ExitCode::from(status);
        }
    };
    let (words, named) = here_files(words);
    let (words, mut cid) = match here_cidfile(words) {
        Ok(rewritten) => rewritten,
        Err((run, why)) => {
            let (said, status) = refused_cid(run, why.clone());
            if !crate::cli::request::refused_on_terminal(if run { "run" } else { "create" }, &why) {
                let _ = writeln!(io::stderr(), "{said}");
            }
            return ExitCode::from(status);
        }
    };
    let words_sent = words.clone();
    let bytes: Vec<Vec<u8>> = words.into_iter().map(String::into_bytes).collect();
    // Of this client's own variables, what shapes the command; and of its credentials, those
    // for the registry the command reaches, never all it holds.
    let mut env: Vec<(String, Vec<u8>)> = frame::ENV
        .iter()
        .filter(|name| **name != "SHARDS_AUTH_CONFIG")
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|v| (name.to_string(), v.into_bytes()))
        })
        .collect();
    // A Windows console names no terminal type: one that takes escape sequences is
    // xterm-256color, as Microsoft's own Linux is told its Windows console is (microsoft/WSL
    // 4609b0c, src/windows/service/exe/DistributionRegistration.h, TERM's default), where
    // this client's TERM says nothing.
    #[cfg(windows)]
    if std::env::var_os("TERM").is_none() && crate::cli::terminal::styles(1) {
        env.push(("TERM".to_string(), b"xterm-256color".to_vec()));
    }
    match own_credentials(&words_sent, &|name| std::env::var(name).ok()) {
        Ok((sent, warnings)) => {
            for warning in warnings {
                let _ = writeln!(io::stderr(), "{warning}");
            }
            if let Some(sent) = sent {
                env.push(("SHARDS_AUTH_CONFIG".to_string(), sent.into_bytes()));
            }
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "{e}");
            return ExitCode::FAILURE;
        }
    }
    // This client's terminal is the command's, its own there for what it is here: raw here
    // while the command reads it, as `ssh -t` leaves the remote terminal to do what a
    // terminal does (OpenSSH clientloop.c, enter_raw_mode). Its size and the signals this
    // process receives go to the command as they come. A file here the command writes or
    // reads is its stdout or stdin instead.
    let terminal = here_terminal().and_then(|mut t| {
        match &named {
            Some(Named::Written { .. }) => t.stdout = false,
            Some(Named::Read(_)) => t.stdin = false,
            Some(Named::WrittenAfter { .. }) | None => {}
        }
        (t.stdin || t.stdout || t.stderr).then_some(t)
    });
    #[cfg(windows)]
    let size_fd = match steer_console(terminal) {
        Ok(fd) => fd,
        Err(e) => return fail(e),
    };
    #[cfg(unix)]
    {
        let reads = terminal.is_some_and(|t| t.stdin);
        let fd = terminal.map(|t| {
            if t.stdin {
                0
            } else if t.stdout {
                1
            } else {
                2
            }
        });
        if let Err(e) = steer_signals(reads, fd) {
            return fail(e);
        }
    }
    // A build of a context here is the machine's of its copy of it, kept in step.
    let stdin_file = || {
        use io::Read as _;
        let mut text = Vec::new();
        io::stdin()
            .take(64 << 20)
            .read_to_end(&mut text)
            .map_err(|e| format!("reading stdin: {e}"))?;
        Ok(text)
    };
    match building(&words_sent, &stdin_file) {
        Ok(Some(built)) => return build_there(&identity, context, built, env),
        Ok(None) => {}
        Err(e) => return fail(e),
    }
    // An artifact made of a directory here, and signed by this client's key or identity.
    if let Some(built) = artifact_building(&words_sent) {
        return build_there(&identity, context, built, env);
    }
    if let Some(code) = artifact_sign_there(&identity, context, &words_sent) {
        return code;
    }
    // A copy is driven here, as this machine's CLI drives one, each of its steps asked of the
    // machine's daemon: its files here are this client's. What steers a command steers no
    // step, so a signal ends the client, and the step there with its stream.
    #[cfg(unix)]
    if let Some(parsed) = copying(&words_sent) {
        return crate::cli::cp::copy(&parsed, &|argv, fds| step_there(&identity, context, argv, fds));
    }
    // The file is made once the signals that would end the client are taken, and named
    // while it is made, so that one that ends the client removes it.
    let mut written = None;
    let mut stdin: Box<dyn io::Read + Send> = Box::new(io::stdin());
    let (gone_tx, gone_rx) = std::sync::mpsc::channel();
    let mut stdout: Box<dyn io::Write + Send> = Box::new(Printed {
        out: io::stdout(),
        gone: gone_tx,
    });
    let mut then = None;
    let (failed_tx, failed_rx) = std::sync::mpsc::channel();
    match &named {
        Some(Named::Written { path, failed } | Named::WrittenAfter { path, failed, .. }) => {
            let mut unfinished = UNFINISHED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let made = crate::cli::save::to_file(path, failed).and_then(|output| {
                let file = output
                    .handle()
                    .ok_or_else(|| failed.to_string())?
                    .try_clone()
                    .map_err(|e| format!("{failed}: {}", crate::cli::save::go(&e)))?;
                *unfinished = output.temp().map(Path::to_path_buf);
                Ok((output, file))
            });
            drop(unfinished);
            match made {
                Ok((output, file)) => {
                    let recorded = Box::new(Recorded {
                        file,
                        failed: failed_tx,
                    });
                    match &named {
                        Some(Named::WrittenAfter { then: words, .. }) => {
                            then = Some((words.clone(), recorded))
                        }
                        _ => stdout = recorded,
                    }
                    written = Some((output, *failed));
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "{e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        Some(Named::Read(path)) => match crate::cli::save::open(path) {
            Ok(file) => stdin = Box::new(file),
            Err(e) => {
                let _ = writeln!(io::stderr(), "{e}");
                return ExitCode::FAILURE;
            }
        },
        None => {}
    }
    if terminal.is_some_and(|t| t.stdin)
        && let Err(e) = crate::cli::terminal::make_raw()
    {
        let terminal = if cfg!(windows) {
            "the console"
        } else {
            "the terminal"
        };
        return fail(format!("{terminal}: {e}"));
    }
    // A raw console's keys are read as its records, which carry its resizes too, and
    // its ^Z, which Rust's reader of its text would take for the end of input.
    #[cfg(windows)]
    if terminal.is_some_and(|t| t.stdin)
        && let Some(input) = crate::cli::terminal::ConsoleInput::new(move || {
            if let Some(fd) = size_fd {
                send_size(fd);
            }
        })
    {
        stdin = Box::new(input);
    }
    let request = Request {
        args: bytes,
        env: env.clone(),
        terminal,
    };
    // Given once the command is asked for: a signal before then ends the client, as it
    // would have, and a resize meanwhile is sent now, as no signal could send it then.
    #[cfg(unix)]
    let sized = terminal.map(|t| {
        (
            if t.stdin {
                0
            } else if t.stdout {
                1
            } else {
                2
            },
            t.rows,
            t.cols,
        )
    });
    let steered = move |controls: client::Controls| {
        let mut steered = STEERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(unix)]
        if let Some((fd, rows, cols)) = sized {
            let now = crate::cli::terminal::size(fd);
            if now != (rows, cols) {
                let _ = controls.resize(now.0, now.1);
            }
        }
        *steered = Some(controls);
        #[cfg(windows)]
        {
            drop(steered);
            if let Some(fd) = size_fd {
                send_size(fd);
            }
        }
    };
    // The ID of what it made, written to this client's `--cidfile` as it comes; one the file
    // cannot take fails the command as here, which there goes on no more.
    let mut unwritten = None;
    let made = |id: &[u8]| {
        if let Some(cid) = &mut cid
            && let Err(e) = cid.file.write(&String::from_utf8_lossy(id))
        {
            unwritten = Some(cid.refused(e));
            if let Some(controls) = STEERED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                let _ = controls.hang_up();
            }
        }
    };
    let mut ran = client::exec_made(
        &identity,
        context.remote(),
        request,
        Some(stdin),
        stdout,
        io::stderr(),
        steered,
        made,
    );
    crate::cli::terminal::restore();
    if let Some((said, status)) = unwritten {
        let _ = writeln!(io::stderr(), "{said}");
        return ExitCode::from(status);
    }
    // What writes the file once the command is through, which was asked for it alone.
    if let (Some((words, recorded)), Ok(Ok(0))) = (then, &ran) {
        let request = Request {
            args: words.into_iter().map(String::into_bytes).collect(),
            env,
            terminal: None,
        };
        let steered = |controls| {
            *STEERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(controls);
        };
        ran = client::exec(
            &identity,
            context.remote(),
            request,
            Some(Box::new(io::empty())),
            recorded,
            io::stderr(),
            steered,
        );
    }
    // The file is kept once the command's exit says it is whole and every byte of it was
    // written here; else it goes, as a save here leaves nothing that is not whole.
    let kept = written.map(|(output, failed)| {
        let mut unfinished = UNFINISHED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = output.temp().map(|t| t.display().to_string()).unwrap_or_default();
        // A write that failed here ended the command there: said in the save's words.
        let kept = match (&ran, failed_rx.try_recv()) {
            (_, Ok(e)) => Err(format!("{failed}: write {temp}: {e}")),
            (Ok(Ok(status)), Err(_)) => output.finish(u8::try_from(*status).unwrap_or(1)),
            _ => Ok(()),
        };
        *unfinished = None;
        kept
    });
    match (ran, kept) {
        (_, Some(Err(e))) => {
            let _ = writeln!(io::stderr(), "{e}");
            ExitCode::FAILURE
        }
        (Ok(Ok(status)), _) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        (Ok(Err(refused)), _) => fail(format!("{}: {refused}", context.name)),
        (Err(_), _) if gone_rx.try_recv().is_ok() => ExitCode::FAILURE,
        (Err(e), _) => fail(format!("{}: {e}", context.name)),
    }
}

/// `words`' copy, read as `cp` reads it (cli/cp.rs), where the command is one; a command
/// shards' parser refuses, or `-h`, goes as it is.
#[cfg(unix)]
fn copying(words: &[String]) -> Option<shards_cmdline::flags::Parsed> {
    let said = shards_cmdline::grammar::rewrite(words).unwrap_or_else(|| words.to_vec());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let (command, path, n) = shards_cmdline::commands::find(&refs)?;
    if !std::ptr::eq(command, &shards_cmdline::commands::COPY) {
        return None;
    }
    let rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    match shards_cmdline::flags::parse(command, path, &rest, &shards_cmdline::flags::value) {
        shards_cmdline::flags::Outcome::Run(parsed) => Some(parsed),
        _ => None,
    }
}

/// A step of a copy (cli/cp.rs) asked of `context`'s machine (D180 part 3b): `argv` what the
/// CLI there would ask its daemon, the step's stream its descriptors, an archive's written
/// into the one in `fds` and an extract's read from it; else what it said on stdout, and on
/// stderr, taken whole, with its status.
#[cfg(unix)]
fn step_there(
    identity: &Identity,
    context: &Context,
    argv: Vec<String>,
    fds: &[crate::cli::Stream<'_>],
) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
    let what = argv.get(1).cloned().unwrap_or_default();
    let owned = |stream: &crate::cli::Stream<'_>| {
        stream
            .fd()
            .try_clone_to_owned()
            .map(fs::File::from)
            .map_err(|e| format!("the copy's pipe: {e}"))
    };
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    let stdin: Box<dyn io::Read + Send> = match (what.as_str(), fds.first()) {
        ("extract", Some(fd)) => Box::new(owned(fd)?),
        _ => Box::new(io::empty()),
    };
    let stdout: Box<dyn io::Write + Send> = match (what.as_str(), fds.first()) {
        ("archive", Some(fd)) => Box::new(owned(fd)?),
        _ => Box::new(Taken(out_tx)),
    };
    let request = Request {
        args: argv.into_iter().map(String::into_bytes).collect(),
        env: Vec::new(),
        terminal: None,
    };
    let ran = client::exec(
        identity,
        context.remote(),
        request,
        Some(stdin),
        stdout,
        Taken(err_tx),
        |_| {},
    );
    let taken = |rx: std::sync::mpsc::Receiver<Vec<u8>>| rx.try_iter().flatten().collect::<Vec<u8>>();
    match ran {
        Ok(Ok(status)) => Ok((u8::try_from(status).unwrap_or(1), taken(out_rx), taken(err_rx))),
        Ok(Err(refused)) => Err(format!("{}: {refused}", context.name)),
        Err(e) => Err(format!("{}: {e}", context.name)),
    }
}

/// What a step writes, sent to its taker as it comes, taken whole once the step is through.
struct Taken(std::sync::mpsc::Sender<Vec<u8>>);

impl io::Write for Taken {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Whether `words` ask for help: `-h` or `--help`, where it is no flag's value (a flag of
/// `valued` takes the word after it) and before `--`.
pub(crate) fn asks_help(words: &[&str], valued: &[&str]) -> bool {
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match *word {
            "-h" | "--help" => return true,
            "--" => return false,
            w if valued.contains(&w) => {
                words.next();
            }
            _ => {}
        }
    }
    false
}

/// `usage`, a command's page, as its help: on a colour terminal laid out as `name`'s.
pub(crate) fn help_page(name: &str, usage: &str) -> ExitCode {
    if crate::cli::look::usage_page(name, usage) {
        return ExitCode::SUCCESS;
    }
    let _ = writeln!(io::stdout(), "{usage}");
    ExitCode::SUCCESS
}

/// `shards context ...`: the contexts this home joined.
pub(crate) fn command(args: &[OsString]) -> ExitCode {
    let home = match shards_ipc::home() {
        Ok(home) => home,
        Err(e) => return failed(&e),
    };
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).collect();
    if asks_help(&words, &["--invite-file", "--description"]) {
        return help_page("context", CONTEXT_USAGE);
    }
    let result = match words.as_slice() {
        ["create", name, rest @ ..] => create(&home, name, rest),
        ["use", name] => use_context(&home, name),
        ["ls" | "list"] => list_contexts(&home),
        ["inspect", names @ ..] if !names.is_empty() => inspect(&home, names),
        ["rm" | "remove", names @ ..] if !names.is_empty() => remove(&home, names),
        ["show"] => {
            let _ = writeln!(io::stdout(), "{}", current(&home));
            Ok(())
        }
        _ => Err(CONTEXT_USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

const CONTEXT_USAGE: &str = "usage: shards create context NAME --invite-file FILE [--description TEXT]
       shards use context NAME
       shards list contexts
       shards inspect context NAME...
       shards remove context NAME...
  A context is another machine's shards, which this one's commands run on whole: its
  microVMs, images, volumes and files. Its owner invites this machine (shards invite
  client), and the invitation's file enrolls it once. --context NAME, SHARDS_CONTEXT or
  DOCKER_CONTEXT choose one for a command; `default` is this machine.";

fn failed(why: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "shards: {why}");
    ExitCode::FAILURE
}

fn create(home: &Path, name: &str, rest: &[&str]) -> Result<(), String> {
    if name == DEFAULT || !valid_name(name) {
        return Err(format!(
            "context name {name:?} is invalid, names are validated against regexp \"^[a-zA-Z0-9][a-zA-Z0-9_.+-]+$\""
        ));
    }
    // Both flags, each at most once, in any order.
    let mut file = None;
    let mut description = None;
    let mut i = 0;
    while let Some(word) = rest.get(i) {
        match *word {
            "--invite-file" => {
                file = Some(*rest.get(i + 1).ok_or("flag needs an argument: --invite-file")?);
                i += 2;
            }
            "--description" => {
                description = Some(*rest.get(i + 1).ok_or("flag needs an argument: --description")?);
                i += 2;
            }
            "--docker" | "--from" => {
                return Err(format!(
                    "{word}: a shards context is another machine's shards, joined by its owner's invitation (--invite-file); it reaches no Docker daemon"
                ));
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let file = file.ok_or("shards create context NAME --invite-file FILE: the invitation the machine's owner made (shards invite client)")?;
    if load(home, name)?.is_some() {
        return Err(format!("context {name:?} already exists"));
    }
    let text = fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    let invitation = Invitation::parse(&text).map_err(|e| format!("{file}: {e}"))?;
    let identity = identity(home)?;
    match client::join(&identity, &invitation) {
        Ok(Ok(())) => {}
        Ok(Err(refused)) => return Err(format!("{}: {refused}", invitation.address)),
        Err(e) => return Err(e.to_string()),
    }
    let context = Context {
        name: name.to_string(),
        description: description.unwrap_or_default().to_string(),
        address: invitation.address,
        daemon: invitation.daemon,
    };
    write_new(&dir(home, "contexts")?.join(name), context.text().as_bytes())?;
    let _ = writeln!(io::stdout(), "{name}");
    let _ = writeln!(io::stderr(), "Successfully created context {name:?}");
    Ok(())
}

fn use_context(home: &Path, name: &str) -> Result<(), String> {
    if name != DEFAULT && load(home, name)?.is_none() {
        return Err(format!("context {name:?} does not exist"));
    }
    shards_vmm::platform::create_private_dir(home).map_err(at(home))?;
    replace(&home.join("context"), format!("{name}\n").as_bytes())?;
    let _ = writeln!(io::stdout(), "{name}");
    let _ = writeln!(io::stderr(), "Current context is now {name:?}");
    Ok(())
}

fn list_contexts(home: &Path) -> Result<(), String> {
    let current = current(home);
    let mut rows = vec![(
        format!("{DEFAULT}{}", if current == DEFAULT { " *" } else { "" }),
        "this machine's shards".to_string(),
        own_endpoint(home),
    )];
    for c in list(home)? {
        let mark = if c.name == current { " *" } else { "" };
        rows.push((
            format!("{}{mark}", c.name),
            c.description,
            format!("quic://{}", c.address),
        ));
    }
    let width = |f: &dyn Fn(&(String, String, String)) -> usize, header: usize| {
        rows.iter().map(f).max().unwrap_or(0).max(header)
    };
    let (a, b) = (width(&|r| r.0.len(), 4), width(&|r| r.1.len(), 11));
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{:a$}   {:b$}   ENDPOINT", "NAME", "DESCRIPTION");
    for (name, description, endpoint) in rows {
        let _ = writeln!(out, "{name:a$}   {description:b$}   {endpoint}");
    }
    Ok(())
}

fn inspect(home: &Path, names: &[&str]) -> Result<(), String> {
    let mut docs = Vec::new();
    for name in names {
        let c = load(home, name)?.ok_or_else(|| format!("context {name:?} does not exist"))?;
        docs.push(serde_json::json!({
            "Name": c.name,
            "Description": c.description,
            "Endpoint": format!("quic://{}", c.address),
            "Daemon": identity::hex(&c.daemon),
            // Where Docker's tools reach its machine's engine, while this home's daemon runs.
            "Engine": engine_endpoint(home, &c.name),
        }));
    }
    let text = serde_json::to_string_pretty(&docs).map_err(|e| e.to_string())?;
    let _ = writeln!(io::stdout(), "{text}");
    Ok(())
}

fn remove(home: &Path, names: &[&str]) -> Result<(), String> {
    for name in names {
        if load(home, name)?.is_none() {
            return Err(format!("context {name:?} does not exist"));
        }
        let path = home.join("contexts").join(name);
        fs::remove_file(&path).map_err(at(&path))?;
        if current(home) == *name {
            let _ = fs::remove_file(home.join("context"));
        }
        // Its engine's socket goes with it, which the daemon that serves it, watching the
        // contexts, lets go of; one no daemon serves is removed here.
        let _ = fs::remove_file(home.join(engine_socket(name)));
        let _ = writeln!(io::stdout(), "{name}");
    }
    Ok(())
}

// What the machine that runs the microVMs keeps: its clients and invitations.

const CLIENT: &str = "shards client 1";
const INVITATION: &str = "shards invitation record 1";

/// A client this machine's daemon enrolled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Client {
    pub name: String,
    pub key: Vec<u8>,
    pub enrolled: u64,
}

fn read_client(path: &Path) -> Option<Client> {
    let text = fs::read_to_string(path).ok()?;
    let fields = fields(&text, CLIENT)?;
    let hex = field(&fields, "key")?;
    let key = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some(Client {
        name: field(&fields, "name")?.to_string(),
        key,
        enrolled: field(&fields, "enrolled")?.parse().ok()?,
    })
}

/// Every client this machine enrolled, by name.
pub(crate) fn clients(home: &Path) -> Vec<Client> {
    let mut out: Vec<Client> = fs::read_dir(home.join("clients"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_client(&e.path()))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Enrolls `key` as client `name`: the daemon's, which listens on Unix alone.
#[cfg(unix)]
pub(crate) fn enroll(home: &Path, name: &str, key: &[u8]) -> Result<(), String> {
    let text = format!(
        "{CLIENT}\nname {name}\nkey {}\nenrolled {}\n",
        identity::hex(key),
        now()
    );
    write_new(&dir(home, "clients")?.join(name), text.as_bytes())
}

/// The invitation `name` this machine made, and when, if it holds one.
pub(crate) fn invitation(home: &Path, name: &str) -> Option<(Invitation, u64)> {
    if !invite::valid_name(name) {
        return None;
    }
    let text = fs::read_to_string(home.join("invitations").join(name)).ok()?;
    let (head, rest) = text.split_once('\n')?;
    let (header, created) = (head, rest.lines().next()?);
    if header != INVITATION {
        return None;
    }
    let created = created.strip_prefix("created ")?.parse().ok()?;
    let body = rest.split_once('\n')?.1;
    Some((Invitation::parse(body).ok()?, created))
}

/// Whether an invitation made at `created` has lapsed by now.
pub(crate) fn lapsed(created: u64) -> bool {
    now().saturating_sub(created) >= invite::LIFETIME_SECS
}

/// The invitations this machine holds, each with when it was made, by name.
fn invitations(home: &Path) -> Vec<(Invitation, u64)> {
    let mut names: Vec<String> = fs::read_dir(home.join("invitations"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names.iter().filter_map(|name| invitation(home, name)).collect()
}

/// How many of this machine's invitations are open: unspent, and not lapsed.
#[cfg(unix)]
pub(crate) fn open_invitations(home: &Path) -> usize {
    invitations(home)
        .iter()
        .filter(|(_, created)| !lapsed(*created))
        .count()
}

/// Redeems invitation `name`: removed, so that it enrolls one client alone.
pub(crate) fn redeem(home: &Path, name: &str) -> Result<(), String> {
    let path = home.join("invitations").join(name);
    fs::remove_file(&path).map_err(at(&path))
}

/// Where this home's daemon listens for clients, while it does.
pub(crate) const LISTENING: &str = "listening";

/// `shards client ...` and `shards invitation ...`: who may join this machine, and who has.
pub(crate) fn admin(thing: &str, args: &[OsString]) -> ExitCode {
    let home = match shards_ipc::home() {
        Ok(home) => home,
        Err(e) => return failed(&e),
    };
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).collect();
    if asks_help(&words, &["--name", "--output"]) {
        return help_page(thing, ADMIN_USAGE);
    }
    let result = match (thing, words.as_slice()) {
        ("client", ["invite", rest @ ..]) => invite_client(&home, rest),
        ("client", ["ls" | "list"]) => {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "NAME\tKEY\tENROLLED");
            for c in clients(&home) {
                let key = identity::hex(&identity::fingerprint(&c.key));
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}",
                    c.name,
                    key.get(..12).unwrap_or_default(),
                    c.enrolled
                );
            }
            Ok(())
        }
        ("client", ["rm" | "remove", names @ ..]) if !names.is_empty() => names.iter().try_for_each(|name| {
            let path = home.join("clients").join(name);
            if !invite::valid_name(name) || !path.is_file() {
                return Err(format!("no client {name:?}"));
            }
            fs::remove_file(&path).map_err(at(&path))?;
            // The copies of its build contexts this machine kept go with it (crate::synced).
            let copies = home.join("synced").join(name);
            if copies.exists() {
                fs::remove_dir_all(&copies).map_err(at(&copies))?;
            }
            let _ = writeln!(io::stdout(), "{name}");
            Ok(())
        }),
        ("invitation", ["ls" | "list"]) => {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "NAME\tADDRESS\tMADE\tLAPSED");
            for (i, created) in invitations(&home) {
                let _ = writeln!(out, "{}\t{}\t{created}\t{}", i.name, i.address, lapsed(created));
            }
            Ok(())
        }
        ("invitation", ["rm" | "remove", names @ ..]) if !names.is_empty() => {
            names.iter().try_for_each(|name| {
                if invitation(&home, name).is_none() {
                    return Err(format!("no invitation {name:?}"));
                }
                redeem(&home, name)?;
                let _ = writeln!(io::stdout(), "{name}");
                Ok(())
            })
        }
        _ => Err(ADMIN_USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

const ADMIN_USAGE: &str = "usage: shards invite client --name NAME --output FILE
       shards list clients
       shards remove client NAME...
       shards list invitations
       shards remove invitation NAME...
  On the machine that runs the microVMs, whose daemon listens for clients (shards run
  daemon --listen ADDR:PORT): an invitation, a private file that enrolls one client as
  NAME and lapses an hour after it is made, and the clients enrolled.";

fn invite_client(home: &Path, rest: &[&str]) -> Result<(), String> {
    let mut name = None;
    let mut output = None;
    let mut i = 0;
    while let Some(word) = rest.get(i) {
        match *word {
            "--name" => {
                name = Some(*rest.get(i + 1).ok_or("flag needs an argument: --name")?);
                i += 2;
            }
            "--output" | "-o" => {
                output = Some(*rest.get(i + 1).ok_or("flag needs an argument: --output")?);
                i += 2;
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let (Some(name), Some(output)) = (name, output) else {
        return Err(ADMIN_USAGE.to_string());
    };
    let listening = fs::read_to_string(home.join(LISTENING))
        .ok()
        .and_then(|a| a.trim().parse::<SocketAddr>().ok())
        .filter(|_| daemon_serves(home))
        .ok_or("this machine's daemon does not listen for clients: start it with `shards run daemon --listen ADDR:PORT`")?;
    if listening.ip().is_unspecified() {
        return Err(format!(
            "the daemon listens on {listening}, which names no address a client can reach: start it with --listen naming one"
        ));
    }
    if clients(home).iter().any(|c| c.name == name) {
        return Err(format!("a client named {name:?} is enrolled already"));
    }
    if invitation(home, name).is_some_and(|(_, created)| !lapsed(created)) {
        return Err(format!("an invitation named {name:?} is open already"));
    }
    let identity = identity(home)?;
    let made = Invitation::new(name, listening, identity.fingerprint()).map_err(|e| e.to_string())?;
    let record = format!("{INVITATION}\ncreated {}\n{}", now(), made.text());
    let path = dir(home, "invitations")?.join(name);
    let _ = fs::remove_file(&path);
    write_new(&path, record.as_bytes())?;
    made.write(Path::new(output)).map_err(|e| e.to_string())?;
    let _ = writeln!(io::stdout(), "{output}");
    let _ = writeln!(
        io::stderr(),
        "Invited {name}: the invitation in {output} enrolls one client, and lapses in an hour"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A run or create that never pulls asks no credential helper; one that may pull does.
    #[test]
    fn a_run_that_never_pulls_asks_no_credential_helper() {
        let config = shards_testdir::TempDir::new("never-pulls").unwrap();
        std::fs::write(
            config.join("config.json"),
            r#"{"credsStore":"shards-tests-absent"}"#,
        )
        .unwrap();
        let dir = config.to_str().unwrap().to_string();
        let env = |name: &str| (name == "DOCKER_CONFIG").then(|| dir.clone());
        let words = |line: &str| line.split(' ').map(String::from).collect::<Vec<_>>();
        for line in [
            "run --pull never example.com/a:1",
            "run --pull=never -it example.com/a:1 sh",
            "container create --pull never example.com/a:1",
        ] {
            assert_eq!(
                own_credentials(&words(line), &env),
                Ok((None, Vec::new())),
                "{line}"
            );
        }
        for line in ["run example.com/a:1", "create --pull always example.com/a:1"] {
            assert!(own_credentials(&words(line), &env).is_err(), "{line}");
        }
    }

    /// A context's record reads back as written; names are Docker's.
    #[test]
    fn a_contexts_record_reads_back() {
        let c = Context {
            name: "gpu".into(),
            description: "the desktop".into(),
            address: "192.168.1.77:7350".parse().unwrap(),
            daemon: [5; 32],
        };
        assert_eq!(Context::parse(&c.text()), Some(c));
        assert!(valid_name("gpu-desktop.1"));
        assert!(!valid_name("g"));
        assert!(!valid_name("-gpu"));
        assert!(!valid_name("gpu desktop"));
    }

    /// A console's events are the signals Go's runtime makes of them for a Windows process:
    /// ^C and ^Break SIGINT; its closing, a logoff and a shutdown SIGTERM; no other.
    #[cfg(windows)]
    #[test]
    fn a_consoles_events_are_the_signals_go_makes_of_them() {
        use windows_sys::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        };
        assert_eq!(console_signal(CTRL_C_EVENT), Some(2));
        assert_eq!(console_signal(CTRL_BREAK_EVENT), Some(2));
        for ends in [CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT] {
            assert_eq!(console_signal(ends), Some(15));
        }
        assert_eq!(console_signal(3), None);
    }

    /// What a command line names of the client's is the client's: a variable `-e NAME` its
    /// value here, and `--env-file` and `--label-file` what they hold, ahead of the rest; the
    /// command's own words untouched.
    #[test]
    fn a_clients_variables_and_files_are_its_own() {
        let words = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        let lookup = |name: &str| (name == "SET").then(|| "here".to_string());
        let read = |path: &str, env: bool| -> Result<Vec<String>, String> {
            match (path, env) {
                ("f.env", true) => Ok(vec!["A=1".into(), "B=2".into()]),
                ("l.txt", false) => Ok(vec!["L=x".into()]),
                _ => Err(format!("{path}: not here")),
            }
        };
        let rewritten = here_values(
            words("run -e SET --env=UNSET -eGIVEN=1 --env-file f.env --label-file=l.txt -it alpine -e SET"),
            &lookup,
            &read,
        )
        .unwrap();
        assert_eq!(
            rewritten,
            words("run --env=A=1 --env=B=2 --label=L=x -e SET=here --env=UNSET -eGIVEN=1 -it alpine -e SET")
        );
        let exec = here_values(words("exec -ite SET box sh"), &lookup, &read).unwrap();
        assert_eq!(exec, words("exec -ite SET=here box sh"));
        assert_eq!(
            here_values(words("ps -a"), &lookup, &read).unwrap(),
            words("ps -a")
        );
        // What a file here fails of is said as the command says it.
        assert_eq!(
            here_values(words("run --env-file gone.env alpine"), &lookup, &read),
            Err((
                "shards: gone.env: not here\n\nRun 'shards run --help' for more information".into(),
                125
            ))
        );
        assert_eq!(
            here_values(words("exec --env-file gone.env box sh"), &lookup, &read),
            Err(("gone.env: not here".into(), 1))
        );
        // A seccomp profile's file, read here and sent as its JSON.
        let dir = shards_testdir::TempDir::new("ctx-seccomp").unwrap();
        let profile = dir.join("p.json");
        std::fs::write(&profile, "{ \"defaultAction\": \"SCMP_ACT_ALLOW\" }").unwrap();
        let profile = profile.to_str().unwrap();
        let json = r#"{"defaultAction":"SCMP_ACT_ALLOW"}"#;
        for (given, sent) in [
            (
                format!("run --security-opt seccomp={profile} alpine"),
                format!("run --security-opt seccomp={json} alpine"),
            ),
            (
                format!("create --security-opt=seccomp:{profile} alpine"),
                format!("create --security-opt=seccomp={json} alpine"),
            ),
            (
                "run --security-opt seccomp=unconfined alpine".into(),
                "run --security-opt seccomp=unconfined alpine".into(),
            ),
            (
                "run --security-opt no-new-privileges alpine".into(),
                "run --security-opt no-new-privileges alpine".into(),
            ),
        ] {
            assert_eq!(
                here_values(words(&given), &lookup, &read),
                Ok(words(&sent)),
                "{given}"
            );
        }
        let Err((gone, 125)) = here_values(
            words("run --security-opt seccomp=gone.json alpine"),
            &lookup,
            &read,
        ) else {
            panic!("a profile not here is refused");
        };
        assert!(
            gone.starts_with("shards: opening seccomp profile (gone.json) failed: open gone.json: ")
                && gone.ends_with("\n\nRun 'shards run --help' for more information"),
            "{gone}"
        );
        // And the command there takes the JSON as the profile.
        assert_eq!(crate::cli::request::seccomp_profile(json), Ok(json.to_string()));
    }

    /// What a context carries, and what it refuses until it carries it.
    #[test]
    fn an_artifacts_build_is_of_the_machines_copy_of_its_directory() {
        let dir = shards_testdir::TempDir::new("ctx-artifact").unwrap();
        let path = AsRef::<Path>::as_ref(&dir).to_str().unwrap().to_string();
        let words = |s: &str| {
            s.replace("DIR", &path)
                .split(' ')
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let built = artifact_building(&words("build agent DIR -t a --platform linux/amd64")).unwrap();
        assert_eq!(
            built.words,
            [
                "agent",
                "build",
                crate::synced::CONTEXT,
                "-t",
                "a",
                "--platform",
                "linux/amd64"
            ]
        );
        assert!(built.carried.is_empty());
        // A copy of its own: an image build of the same directory keeps another, of what
        // that build's ignore file leaves; and an MCP server's another again.
        let image = building(&words("build DIR"), &|| Ok(Vec::new()))
            .unwrap()
            .unwrap();
        let mcp = artifact_building(&words("mcp build DIR -t m")).unwrap();
        assert_ne!(built.key, image.key);
        assert_ne!(built.key, mcp.key);
        // Its SBOM's scanner's registry, whose login goes with it.
        let scanned =
            artifact_building(&words("build agent DIR -t a --sbom=generator=ghcr.io/me/scan:1")).unwrap();
        assert_eq!(scanned.domains.into_iter().collect::<Vec<_>>(), ["ghcr.io"]);
        // No directory here, no build of it there: the command says so itself.
        assert!(artifact_building(&words("build agent DIR/none -t a")).is_none());
        assert!(artifact_building(&words("agent ls")).is_none());
    }

    /// A step's agent requests, relayed here, are answered only as a read-only agent answers
    /// them, whatever the machine let by: a request to forget every key refused here, never
    /// reaching an agent that would do it, and a listing relayed.
    #[cfg(unix)]
    #[test]
    fn a_relayed_agent_answers_only_what_a_read_only_agent_does() {
        use std::io::Read as _;
        let dir = shards_testdir::TempDir::new("ctx-agent").unwrap();
        let path = AsRef::<Path>::as_ref(&dir).join("a.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // An agent that does whatever it is asked, and says so.
        let yes = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut asked = Vec::new();
            let mut message = [0u8; 5];
            while conn.read_exact(&mut message).is_ok() {
                asked.push(message[4]);
                conn.write_all(&[0, 0, 0, 1, 6]).unwrap();
            }
            asked
        });
        let agent = shards_cmdline::buildflags::Agent::Socket(path);
        let (asked, mut asking) = io::pipe().unwrap();
        let (mut answers, answering) = io::pipe().unwrap();
        std::thread::scope(|scope| {
            let answered = scope.spawn(|| answer_agent(asked, answering, &agent));
            // SSH_AGENTC_REMOVE_ALL_IDENTITIES, then SSH_AGENTC_REQUEST_IDENTITIES.
            for request in [19u8, 11] {
                asking.write_all(&[0, 0, 0, 1, request]).unwrap();
            }
            drop(asking);
            let mut said = Vec::new();
            answers.read_to_end(&mut said).unwrap();
            answered.join().unwrap().unwrap();
            assert_eq!(said, [0, 0, 0, 1, 5, 0, 0, 0, 1, 6]);
        });
        drop(agent);
        assert_eq!(yes.join().unwrap(), [11], "what reached the agent");
    }

    #[test]
    fn a_context_refuses_what_it_does_not_carry() {
        let args = |s: &str| s.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert!(not_carried(&args("ps -a")).is_none());
        assert!(not_carried(&args("run --rm alpine echo hi")).is_none());
        // The client's terminal crosses (part 2).
        assert!(not_carried(&args("run -it alpine sh")).is_none());
        assert!(not_carried(&args("exec -it web sh")).is_none());
        assert!(not_carried(&args("attach web")).is_none());
        // No command of shards', said so there as here.
        assert!(not_carried(&args("compose up")).is_none());
        // The builder's own, asked of the machine's; a policy's files here, not yet.
        for builder in [
            "buildx du",
            "builder du",
            "buildx prune -f",
            "builder prune -a",
            "buildx policy",
        ] {
            assert!(not_carried(&args(builder)).is_none(), "{builder}");
        }
        for policy in ["buildx policy eval alpine", "buildx policy test ."] {
            assert!(not_carried(&args(policy)).is_some(), "{policy}");
        }
        // An artifact's directory is kept in step there ([`artifact_building`]), and its
        // signature made here ([`artifact_sign_there`]).
        for carried in [
            "build agent dir -t a",
            "agent build dir -t a",
            "sign agent a",
            "harness sign h --key k",
        ] {
            assert!(not_carried(&args(carried)).is_none(), "{carried}");
        }
        // A build's context is kept in step there ([`building`]); what else it names here,
        // not yet.
        for carried in [
            "build .",
            "image build .",
            "builder build .",
            "build image .",
            "build -t a -f d/Dockerfile .",
        ] {
            assert!(not_carried(&args(carried)).is_none(), "{carried}");
        }
        // Its secrets cross in memory, read here; its outputs come back as it ends.
        for carried in [
            "build --secret id=a,src=f .",
            "build -o out .",
            "build -o type=local,dest=out .",
            "build --iidfile i .",
            "build --metadata-file m .",
        ] {
            assert!(not_carried(&args(carried)).is_none(), "{carried}");
        }
        // Its named contexts, local caches and policies' files, kept in step or carried.
        for carried in [
            "build --cache-to type=local,dest=c .",
            "build --build-context base=../dir .",
            "build --policy filename=p.rego .",
        ] {
            assert!(not_carried(&args(carried)).is_none(), "{carried}");
        }
        assert!(not_carried(&args("build --ssh default .")).is_none());
        for remote in [
            "build -o type=registry .",
            "build -o - .",
            "build --build-context base=docker-image://alpine .",
        ] {
            assert!(not_carried(&args(remote)).is_none(), "{remote}");
        }
        // A copy is driven here, where cp.rs runs.
        for copy in ["cp web:/x .", "container cp web:/x .", "copy vm web:/x ."] {
            assert_eq!(not_carried(&args(copy)).is_none(), cfg!(unix), "{copy}");
        }
        assert!(not_carried(&args("agent ls")).is_none());
        // A file a command writes or reads whole crosses as its output or input ([`here_files`]).
        for whole in [
            "save -o x.tar alpine",
            "save image alpine -o x.tar",
            "export -o x.tar web",
            "load -i x.tar",
            "load image -i x.tar",
            "import x.tar",
            "image import x.tar",
        ] {
            assert!(not_carried(&args(whole)).is_none(), "{whole}");
        }
        // A run's `--cidfile` is made here ([`here_cidfile`]).
        for cid in [
            "run --cidfile c alpine",
            "create --cidfile c alpine",
            "run vm --cidfile c alpine sh",
        ] {
            assert!(not_carried(&args(cid)).is_none(), "{cid}");
        }
        // What the client's files hold crosses ([`here_values`]).
        assert!(not_carried(&args("run --env-file e alpine")).is_none());
        assert!(kept_here(&args("context ls")));
        assert!(kept_here(&args("list contexts")));
        assert!(!kept_here(&args("ps")));
    }

    #[test]
    fn shards_own_commands_name_the_registry_they_reach() {
        let named = |s: &str| {
            let refs: Vec<&str> = s.split(' ').collect();
            own_named(&refs).map(str::to_string)
        };
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            named("agentfile -o A --platform linux/amd64 r/i:1"),
            some("r/i:1")
        );
        assert_eq!(named("agentfile r/i:1 --output A"), some("r/i:1"));
        assert_eq!(named("agent push r/a:1"), some("r/a:1"));
        assert_eq!(named("mcp pull r/m:1"), some("r/m:1"));
        assert_eq!(named("harness rm r/h:1 --referrer sha256:ab"), some("r/h:1"));
        assert_eq!(named("agent remove --referrer=sha256:ab r/a:1"), some("r/a:1"));
        // A removal here, and a listing, reach no registry.
        assert_eq!(named("agent rm r/a:1"), None);
        assert_eq!(named("agent ls"), None);
        assert_eq!(named("agentfile -o A"), None);
        assert_eq!(named("agent push"), None);
    }

    #[test]
    fn a_file_a_command_writes_or_reads_whole_is_the_clients() {
        let words = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        let written = |path: &str, failed| {
            Some(Named::Written {
                path: path.into(),
                failed,
            })
        };
        let read = |path: &str| Some(Named::Read(path.into()));
        let save = "failed to save image";
        let export = "failed to export container";
        for (given, sent, named) in [
            // `-o` however it is given, the command writing its stdout instead.
            ("save -o x.tar alpine", "save alpine", written("x.tar", save)),
            ("save -ox.tar alpine", "save alpine", written("x.tar", save)),
            ("save -o=x.tar alpine", "save alpine", written("x.tar", save)),
            (
                "save --output x.tar alpine",
                "save alpine",
                written("x.tar", save),
            ),
            ("save --output=x.tar a b", "save a b", written("x.tar", save)),
            ("image save a -o x.tar", "image save a", written("x.tar", save)),
            // The last one named, as pflag keeps it.
            (
                "save -o a.tar -o b.tar alpine",
                "save alpine",
                written("b.tar", save),
            ),
            // shards' own words, said as the command they run.
            (
                "save image alpine -o x.tar",
                "save --convert alpine",
                written("x.tar", save),
            ),
            ("export -o x.tar web", "export web", written("x.tar", export)),
            ("export vm web -o x.tar", "export web", written("x.tar", export)),
            (
                "container export web --output x.tar",
                "export web",
                written("x.tar", export),
            ),
            (
                "agentfile alpine -o A",
                "agentfile alpine",
                written("A", "failed to write the Agentfile"),
            ),
            (
                "make agentfile alpine --output=A",
                "agentfile alpine",
                written("A", "failed to write the Agentfile"),
            ),
            // `-i`, the flags it ran together with kept.
            ("load -i x.tar", "load", read("x.tar")),
            ("load -qi x.tar", "load -q", read("x.tar")),
            ("load -qix.tar", "load -q", read("x.tar")),
            ("load -qi=x.tar", "load -q", read("x.tar")),
            ("load --input=x.tar -q", "load -q", read("x.tar")),
            ("load image -i x.tar", "load", read("x.tar")),
            ("image load -q --input x.tar", "load -q", read("x.tar")),
            // `import`'s file, read as `-`; past `--` too.
            ("import x.tar app:1", "import - app:1", read("x.tar")),
            ("import -m hi x.tar", "import -m hi -", read("x.tar")),
            ("image import -- -x.tar", "image import -- -", read("-x.tar")),
        ] {
            assert_eq!(here_files(words(given)), (words(sent), named), "{given}");
        }
        // A pull's Agentfile, asked of the machine once the pull is through, of the
        // platform it named.
        let then = |path: &str, then: &str| {
            Some(Named::WrittenAfter {
                path: path.into(),
                failed: "failed to write the Agentfile",
                then: words(then),
            })
        };
        for (given, sent, named) in [
            (
                "pull --output-agentfile A alpine",
                "pull alpine",
                then("A", "agentfile alpine"),
            ),
            (
                "pull -q --platform linux/amd64 --output-agentfile=A alpine",
                "pull -q --platform linux/amd64 alpine",
                then("A", "agentfile --platform linux/amd64 alpine"),
            ),
            (
                "image pull alpine --output-agentfile A",
                "pull alpine",
                then("A", "agentfile alpine"),
            ),
        ] {
            assert_eq!(here_files(words(given)), (words(sent), named), "{given}");
        }
        // Nothing of the client's: stdin, stdout, a URL the daemon fetches, help, a refusal.
        for same in [
            "save alpine",
            "save -o  alpine",
            "load",
            "load -q",
            "import - app",
            "import https://example.com/x.tar",
            "import http://example.com/x.tar",
            "save -h -o x.tar alpine",
            "load -i",
            "save --nope -o x.tar alpine",
            "agentfile alpine",
            "agentfile alpine -o",
            "agentfile -h -o A alpine",
            "save vm web",
            "pull alpine",
            "pull -a --output-agentfile A alpine",
            "ps -a",
            "run --rm alpine save -o x.tar a",
        ] {
            assert_eq!(here_files(words(same)), (words(same), None), "{same}");
        }
    }

    /// Help is `-h` or `--help` wherever it is said, but as a flag's value or after `--`.
    #[test]
    fn help_is_asked_for_anywhere_but_as_a_value() {
        assert!(asks_help(&["remove", "-h"], &[]));
        assert!(asks_help(
            &["create", "x", "--invite-file", "f", "--help"],
            &["--invite-file"]
        ));
        assert!(!asks_help(
            &["create", "x", "--description", "-h"],
            &["--description"]
        ));
        assert!(!asks_help(&["--", "-h"], &[]));
        assert!(!asks_help(&["remove", "x"], &[]));
    }
}
