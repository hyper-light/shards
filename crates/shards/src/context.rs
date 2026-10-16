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

/// The exit status of a command shards did not run, as Docker's CLI exits for an error
/// before its command runs.
const NOT_RUN: u8 = 125;

/// The variable a daemon sets for a client's command on a terminal of its own: the command
/// takes the terminal as its session's controlling terminal, first thing, as a login's
/// command has its own (D180), which no action of posix_spawn(3) makes on every system.
#[cfg(unix)]
pub(crate) const TAKE_TERMINAL: &str = "SHARDS_TAKE_TERMINAL";

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

/// A console has no SIGWINCH: its size is looked at every 250 ms, as docker/cli's
/// MonitorTtySize looks at a Windows console's (cli/command/container/tty.go), and a change
/// sizes the command's terminal. Its events, ^C and ^Break where its input is not raw, and
/// its closing, go to the command as Go's runtime gives them to a Windows process
/// ([`console_signal`]).
#[cfg(windows)]
fn steer_console(terminal: Option<frame::Terminal>) -> Result<(), String> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: a handler of this process's console's events, which it never removes. A
    // process with no console has no events to take: a handler it could not have is none.
    let _ = unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) };
    let Some(terminal) = terminal else {
        return Ok(());
    };
    let fd = match (terminal.stdout, terminal.stderr) {
        (true, _) => 1,
        (false, true) => 2,
        (false, false) => return Ok(()),
    };
    let mut last = (terminal.rows, terminal.cols);
    std::thread::Builder::new()
        .name("resizes".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let now = crate::cli::terminal::size(fd);
                if now == last {
                    continue;
                }
                last = now;
                let steered = STEERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(controls) = steered.as_ref()
                    && controls.resize(now.0, now.1).is_err()
                {
                    return;
                }
            }
        })
        .map_err(|e| format!("resize thread: {e}"))?;
    Ok(())
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
/// instead, as the Docker CLI's go to its container (`--sig-proxy`). With `terminal`, the
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

/// A context's name, as docker/cli names one (v29.8.1 cli/context/store, ValidateContextName):
/// a letter or digit, then letters, digits, `_`, `.`, `+` or `-`; and not `default`, this
/// machine's.
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
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(at(path))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(at(path))
}

/// `bytes` in place of what `path` holds, whole: written beside it, then renamed over it.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let new = path.with_extension(format!("new-{}", std::process::id()));
    let _ = fs::remove_file(&new);
    write_new(&new, bytes)?;
    fs::rename(&new, path).map_err(at(path))
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
fn list(home: &Path) -> Result<Vec<Context>, String> {
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
/// `DOCKER_CONTEXT`, else the home's current one, as docker/cli chooses (v29.8.1
/// cli/command/cli.go, resolveContextName); `default` is this machine.
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
    if matches!(
        first,
        "build" | "buildx" | "builder" | "load" | "import" | "cp" | "compose"
    ) || refs
        .get(..2)
        .is_some_and(|w| matches!(w, ["container" | "image", "cp" | "load" | "import"]))
    {
        return Some(format!(
            "{first} over a context sends this machine's files, which shards does not carry yet: run it on the machine itself"
        ));
    }
    // `run` is the CLI's own to read (request.rs), as Docker's flags are its.
    let (command, path, n) = match refs.as_slice() {
        ["run", ..] => (&shards_cmdline::commands::RUN, "shards run", 1),
        ["container", "run", ..] => (&shards_cmdline::commands::RUN, "shards container run", 2),
        _ => shards_cmdline::commands::find(&refs)?,
    };
    let rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) =
        shards_cmdline::flags::parse(command, path, &rest, &|_, value| Ok(value.to_string()))
    else {
        return None;
    };
    let files = !parsed.many("env-file").is_empty()
        || !parsed.many("label-file").is_empty()
        || !parsed.string("cidfile").is_empty()
        || !parsed.string("output").is_empty();
    files.then(|| format!("{path}: a file of this machine's, which a context does not carry yet"))
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
    words.is_empty()
        || words
            .first()
            .is_some_and(|w| own(w) || matches!(*w, "help" | "-h" | "--help"))
        || words.get(1).is_some_and(|w| own(w))
}

/// Whether this home's daemon serves now.
fn daemon_serves(home: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(home.join(shards_ipc::SOCKET)).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = home;
        false
    }
}

/// Where this home's daemon serves its own CLI.
fn own_endpoint(home: &Path) -> String {
    #[cfg(unix)]
    {
        format!("unix://{}", home.join(shards_ipc::SOCKET).display())
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
    let mut bytes = Vec::with_capacity(args.len());
    for arg in &args {
        match arg.to_str() {
            Some(text) => bytes.push(text.as_bytes().to_vec()),
            None => {
                return fail(format!(
                    "{arg:?}: an argument that is not UTF-8 does not cross to another machine"
                ));
            }
        }
    }
    let env = frame::ENV
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|v| (name.to_string(), v.into_bytes()))
        })
        .collect();
    // This client's terminal is the command's, its own there for what it is here: raw here
    // while the command reads it, as `ssh -t` leaves the remote terminal to do what a
    // terminal does (OpenSSH clientloop.c, enter_raw_mode). Its size and the signals this
    // process receives go to the command as they come.
    let terminal = here_terminal();
    #[cfg(windows)]
    {
        if let Err(e) = steer_console(terminal) {
            return fail(e);
        }
        if terminal.is_some_and(|t| t.stdin)
            && let Err(e) = crate::cli::terminal::make_raw()
        {
            return fail(format!("the console: {e}"));
        }
    }
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
        if reads && let Err(e) = crate::cli::terminal::make_raw() {
            return fail(format!("the terminal: {e}"));
        }
    }
    let request = Request {
        args: bytes,
        env,
        terminal,
    };
    let stdin: Box<dyn io::Read + Send> = Box::new(io::stdin());
    let steered = |controls| {
        *STEERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(controls);
    };
    let ran = client::exec(
        &identity,
        context.remote(),
        request,
        Some(stdin),
        io::stdout(),
        io::stderr(),
        steered,
    );
    crate::cli::terminal::restore();
    match ran {
        Ok(Ok(status)) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Ok(Err(refused)) => fail(format!("{}: {refused}", context.name)),
        Err(e) => fail(format!("{}: {e}", context.name)),
    }
}

/// `shards context ...`: the contexts this home joined.
pub(crate) fn command(args: &[OsString]) -> ExitCode {
    let home = match shards_ipc::home() {
        Ok(home) => home,
        Err(e) => return failed(&e),
    };
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).collect();
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
                    "{word}: a shards context is another machine's shards, joined by its owner's invitation (--invite-file); Docker's endpoints are Docker's"
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

    /// What a context carries, and what it refuses until it carries it.
    #[test]
    fn a_context_refuses_what_it_does_not_carry() {
        let args = |s: &str| s.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert!(not_carried(&args("ps -a")).is_none());
        assert!(not_carried(&args("run --rm alpine echo hi")).is_none());
        // The client's terminal crosses (part 2).
        assert!(not_carried(&args("run -it alpine sh")).is_none());
        assert!(not_carried(&args("exec -it web sh")).is_none());
        assert!(not_carried(&args("attach web")).is_none());
        assert!(not_carried(&args("build .")).is_some());
        assert!(not_carried(&args("save -o x.tar alpine")).is_some());
        assert!(not_carried(&args("run --env-file e alpine")).is_some());
        assert!(
            not_carried(&args("run --rm alpine cat --env-file e")).is_none(),
            "the command's own --env-file"
        );
        assert!(
            not_carried(&args("run --name web --env-file e alpine sh")).is_some(),
            "past a flag's value"
        );
        assert!(
            not_carried(&args("run vm --env-file e alpine sh")).is_some(),
            "shards' own words"
        );
        assert!(kept_here(&args("context ls")));
        assert!(kept_here(&args("list contexts")));
        assert!(!kept_here(&args("ps")));
    }
}
