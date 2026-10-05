//! The command line: `run` and the container and image commands read as the Docker CLI
//! reads them, their `--help` and usage mistakes answered here, the rest asked of the
//! daemon; `shards daemon stop` stops it; `shards run --kernel` and `shards restore` become the VM process; every other
//! command is the daemon side's (main.rs, `shardsd`), in this process. One binary does it
//! all and starts as fast as the command alone did: it links no framework that loads at
//! launch, binding Apple's when first needed (shards_apple, the VMM's hvf::ffi; PM M113).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use shards_cmdline::flags::{self, Command, Outcome, Parsed};

#[cfg(unix)]
mod client;
#[cfg(unix)]
pub(crate) mod look;
mod request;
#[cfg(unix)]
mod save;
#[cfg(unix)]
mod screens;
#[cfg(unix)]
mod show;
#[cfg(unix)]
mod terminal;

/// `docker run`'s status when it could not run the command at all.
const NOT_RUN: u8 = 125;

pub fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    dispatch(args)
}

/// The commands shards has besides the catalog's: the daemon side's own words.
/// `daemon` and `guest` are what `run daemon`, `stop daemon` and `configure guest` are
/// said as, and how the client starts the daemon.
const OWN: [&str; 10] = [
    "daemon",
    "guest",
    "grants",
    "--version",
    "builder",
    "buildx",
    "help",
    "-h",
    "--help",
    "restore",
];

fn dispatch(args: Vec<OsString>) -> ExitCode {
    // shards' own grammar, `ACTION THING ...`, said as the command it runs.
    let text: Vec<String> = args
        .iter()
        .map_while(|a| a.to_str().map(str::to_string))
        .collect();
    if text.len() == args.len()
        && let Some(said) = shards_cmdline::grammar::rewrite(&text)
        && said != text
    {
        return dispatch(said.into_iter().map(OsString::from).collect());
    }
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).take(2).collect();
    match words.as_slice() {
        [] | ["help" | "-h" | "--help"] => return top_help(),
        // `shards help pull` is `shards pull --help`.
        ["help", ..] => {
            let mut rest: Vec<OsString> = args.iter().skip(1).cloned().collect();
            rest.push("--help".into());
            return dispatch(rest);
        }
        [name @ ("image" | "container")] | [name @ ("image" | "container"), "-h" | "--help"] => {
            return management_help(name);
        }
        [name @ ("image" | "container"), word, ..]
            if shards_cmdline::commands::find(&words).is_none()
                && shards_cmdline::commands::build(&words).is_none()
                && !matches!(*word, "run" | "exec") =>
        {
            return unknown(&shards_cmdline::catalog::unknown_in(name, word));
        }
        [word, ..]
            if !word.starts_with('-')
                && !OWN.contains(word)
                && !shards_cmdline::catalog::TOP
                    .iter()
                    .any(|g| g.entries.iter().any(|e| e.name == *word)) =>
        {
            return unknown(&shards_cmdline::catalog::unknown(word));
        }
        _ => {}
    }
    match words.as_slice() {
        // `shards run --kernel FILE ...`: a kernel booted directly, in the VM process.
        ["run", "--kernel", ..] => {
            let mut boot = vec![OsString::from("run")];
            boot.extend(args.iter().skip(1).cloned());
            vm(&boot)
        }
        ["run", ..] => request::run("shards run", args.get(1..).unwrap_or_default()),
        ["container", "run", ..] => request::run("shards container run", args.get(2..).unwrap_or_default()),
        ["exec", ..] => request::exec("shards exec", args.get(1..).unwrap_or_default()),
        ["container", "exec", ..] => {
            request::exec("shards container exec", args.get(2..).unwrap_or_default())
        }
        #[cfg(unix)]
        ["daemon", "stop"] if args.len() == 2 => match shards_ipc::home() {
            Ok(home) => client::stop(&home),
            Err(e) => failed(&e),
        },
        // `shards restore DIR`: a microVM resumed from its snapshot.
        ["restore", ..] => vm(&args),
        // `build` (or `builder build`, `image build`, `buildx build`, `buildx b`): shardsd's.
        _ if let Some(named) = shards_cmdline::commands::build(&words) => {
            let mut rest = vec![OsString::from("build")];
            rest.extend(args.get(named..).unwrap_or_default().iter().cloned());
            crate::shardsd(rest)
        }
        _ => match shards_cmdline::commands::find(&words) {
            Some((command, path, named)) => container(command, path, &words, named, &args),
            None => crate::shardsd(args.clone()),
        },
    }
}

/// A container command, `named` words of `words` naming it: read here, and run by the
/// daemon.
fn container(
    command: &'static Command,
    path: &str,
    words: &[&str],
    named: usize,
    args: &[OsString],
) -> ExitCode {
    let argv = match utf8(args.get(named..).unwrap_or_default()) {
        Ok(argv) => argv,
        Err(e) => return failed(&e),
    };
    let parsed = match read(command, path, &argv, &|_, value| Ok(value.to_string())) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
    #[cfg(unix)]
    {
        // `save` writes where the client says, opened here, before the client moves to
        // the daemon's home.
        let output = if std::ptr::eq(command, &shards_cmdline::commands::SAVE) {
            match save::output(parsed.string("output")) {
                Ok(output) => Some(output),
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            None
        };
        // `load` reads what the client opens, or its stdin.
        let input = if std::ptr::eq(command, &shards_cmdline::commands::LOAD) {
            match save::input(parsed.string("input")) {
                Ok(input) => Some(input),
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            None
        };
        let stdin = std::io::stdin();
        // The daemon reads the command line again, by the same words.
        let mut argv = argv;
        argv.splice(0..0, words.iter().take(named).map(|w| (*w).to_string()));
        let resolved = shardsd().and_then(|daemon| {
            let identity =
                shards_ipc::Identity::of_build(&daemon).map_err(|e| format!("{}: {e}", daemon.display()))?;
            Ok((daemon, identity, shards_ipc::home()?))
        });
        match resolved {
            Ok((daemon, identity, home)) => {
                use std::os::fd::AsFd as _;
                let mut fds: Vec<std::os::fd::BorrowedFd<'_>> = output.iter().map(save::Output::fd).collect();
                match &input {
                    Some(Some(file)) => fds.push(file.as_fd()),
                    Some(None) => fds.push(stdin.as_fd()),
                    None => {}
                }
                let status = client::container(
                    &home,
                    &daemon,
                    &shards_ipc::Command {
                        argv,
                        registry_env: shards_ipc::registry_env(),
                        east_asian: shards_cmdline::width::east_asian(|name| {
                            std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
                        }),
                        now: now_ns(),
                        utc_offset: utc_offset(),
                        // SAFETY: isatty(3) on this process's stdout.
                        terminal: unsafe { libc::isatty(1) } == 1,
                        width: terminal::size(1).1,
                        // docker/cli's tui.NewOutput: any NO_COLOR but an empty one; and a
                        // terminal that says it is dumb.
                        color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                            && std::env::var_os("TERM").is_none_or(|t| t != "dumb"),
                        daemon: identity,
                    },
                    &fds,
                );
                drop(fds);
                match output.map(|o| o.finish(status)) {
                    Some(Err(e)) => {
                        let _ = writeln!(std::io::stderr(), "{e}");
                        ExitCode::FAILURE
                    }
                    _ => ExitCode::from(status),
                }
            }
            Err(e) => failed(&e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = words;
        // No daemon here yet: a pull runs in this process, said as `docker pull` says it.
        if std::ptr::eq(command, &shards_cmdline::commands::PULL) {
            let home = match shards_ipc::home() {
                Ok(home) => home,
                Err(e) => return failed(&e),
            };
            let out = crate::pull::Out {
                out: &|line| {
                    let _ = writeln!(std::io::stdout(), "{line}");
                },
                err: &|line| {
                    let _ = writeln!(std::io::stderr(), "{line}");
                },
                progress: None,
            };
            let env = |k: &str| std::env::var(k).ok();
            return ExitCode::from(crate::pull::command(&parsed, &home, &env, &out, None));
        }
        failed(
            "container commands need the daemon, which needs Unix sockets, which shards does not support on this platform yet",
        )
    }
}

/// `shards --help`: shards' own page on a colour terminal, docker/cli's text elsewhere.
fn top_help() -> ExitCode {
    #[cfg(unix)]
    if let Some(p) = look::styled() {
        look::top(&p, &mut std::io::stdout().lock());
        return ExitCode::SUCCESS;
    }
    let _ = std::io::stdout().write_all(shards_cmdline::catalog::top().as_bytes());
    ExitCode::SUCCESS
}

/// A management command's help, as [`top_help`] says the root's.
fn management_help(name: &str) -> ExitCode {
    #[cfg(unix)]
    if let Some(p) = look::styled()
        && look::management(&p, name, &mut std::io::stdout().lock())
    {
        return ExitCode::SUCCESS;
    }
    match shards_cmdline::catalog::management_help(name) {
        Some(text) => {
            let _ = std::io::stdout().write_all(text.as_bytes());
            ExitCode::SUCCESS
        }
        None => unknown(&shards_cmdline::catalog::unknown(name)),
    }
}

/// A command shards does not have, refused as docker/cli refuses one (status 1); in a
/// panel on a colour terminal.
fn unknown(text: &str) -> ExitCode {
    #[cfg(unix)]
    if let Some(p) = look::styled_err() {
        let first = text.lines().next().unwrap_or(text);
        let first = first.strip_prefix("shards: ").unwrap_or(first);
        look::error(&p, "shards", first, &["shards --help lists every command"]);
        return ExitCode::FAILURE;
    }
    let _ = writeln!(std::io::stderr(), "{text}");
    ExitCode::FAILURE
}

/// Reads `argv` for `command`, which `path` names; or answers its `--help` or its
/// mistakes as the Docker CLI does, with the status to exit with.
fn read(
    command: &'static Command,
    path: &str,
    argv: &[String],
    validate: &dyn Fn(&flags::Flag, &str) -> Result<String, String>,
) -> Result<Parsed, ExitCode> {
    match flags::parse(command, path, argv, validate) {
        Outcome::Run(parsed) => Ok(parsed),
        Outcome::Help { notices } => {
            let _ = std::io::stdout().write_all(notices.as_bytes());
            #[cfg(unix)]
            if let Some(p) = look::styled() {
                look::command(&p, command, path, &mut std::io::stdout().lock());
                return Err(ExitCode::SUCCESS);
            }
            let help = flags::help(command, path, columns());
            let _ = write!(std::io::stdout(), "{help}");
            Err(ExitCode::SUCCESS)
        }
        Outcome::Fail {
            notices,
            text,
            status,
        } => {
            let _ = std::io::stdout().write_all(notices.as_bytes());
            #[cfg(unix)]
            if let Some(p) = look::styled_err() {
                let first = text.lines().next().unwrap_or(&text);
                let name = path.strip_prefix("shards ").unwrap_or(path);
                let usage = format!("usage: {path} {}", command.usage);
                let more = format!("{path} --help shows its options");
                look::error(&p, name, first, &[usage.trim_end(), &more]);
                return Err(ExitCode::from(status));
            }
            let _ = writeln!(std::io::stderr(), "{text}");
            Err(ExitCode::from(status))
        }
    }
}

/// The width of the terminal on stdin, as the Docker CLI wraps `--help` to it, or 80
/// (docker/cli cli/cobra.go wrappedFlagUsages; on Windows it asks of handle 0, which is
/// never a console, so 80).
fn columns() -> u16 {
    #[cfg(unix)]
    {
        // SAFETY: an all-zero winsize is a valid value.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ fills a winsize, which lives on this stack.
        if unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } == 0 {
            return size.ws_col;
        }
    }
    80
}

/// This clock, in nanoseconds since the epoch.
#[cfg(unix)]
fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// This process's time zone's offset east of UTC now, in seconds, as Go's `time.Now()`
/// has it in its `Local` zone: localtime(3)'s `tm_gmtoff`.
#[cfg(unix)]
fn utc_offset() -> i32 {
    // SAFETY: time(3) and localtime_r(3) write only into the locals given.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut local: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut local).is_null() {
            return 0;
        }
        i32::try_from(local.tm_gmtoff).unwrap_or(0)
    }
}

/// `args` as text, which every command here takes.
fn utf8(args: &[OsString]) -> Result<Vec<String>, String> {
    args.iter()
        .map(|a| {
            a.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("argument {a:?} is not valid UTF-8"))
        })
        .collect()
}

/// The VM process's binary, which `shards` carries and writes out (helpers.rs).
fn vm_binary() -> Result<PathBuf, String> {
    crate::helpers::vm()
}

/// The daemon's binary: this one.
fn shardsd() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("this binary: {e}"))
}

/// Runs the VM process's binary with `args`, in this process's place: the same pid,
/// stdio and signals.
#[cfg(unix)]
fn instead(args: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let bin = match vm_binary() {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    let e = std::process::Command::new(&bin).args(args).exec();
    failed(&format!("{}: {e}", bin.display()))
}

/// Runs the VM process's binary with `args`, and exits as it does: Windows has no exec.
#[cfg(not(unix))]
fn instead(args: &[OsString]) -> ExitCode {
    let bin = match vm_binary() {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    match std::process::Command::new(&bin).args(args).status() {
        Ok(status) => ExitCode::from(status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1)),
        Err(e) => failed(&format!("{}: {e}", bin.display())),
    }
}

/// `shards run --kernel` and `shards restore`: become shards-vm. On macOS, where shards-vm runs in App Sandbox and may
/// open nothing it is not granted, it first starts the VM's broker, `shardsd grants`, on a
/// socket the VM then asks on (`--grants`; docs/research/macos-confinement.md §3). The VM
/// keeps this process: its terminal, its signals, its exit status. The broker leads a
/// session of its own, out of reach of a Ctrl-C meant for the VM, exits once the VM has
/// all it needs, and is reaped by the kernel, since SIGCHLD stays ignored through the exec.
#[cfg(target_os = "macos")]
fn vm(args: &[OsString]) -> ExitCode {
    use std::os::fd::{AsFd as _, IntoRawFd as _};
    if !matches!(args.first().and_then(|a| a.to_str()), Some("run" | "restore")) {
        return instead(args);
    }
    let broker = match shardsd() {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    let (ours, theirs) = match std::os::unix::net::UnixStream::pair() {
        Ok(pair) => pair,
        Err(e) => return failed(&format!("the VM's grants socket: {e}")),
    };
    // SAFETY: signal(2) setting SIGCHLD's disposition, before any thread starts.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    let stderr = std::io::stderr();
    let spawned = shards_ipc::spawn(
        &broker,
        &["grants".as_ref()],
        &[(theirs.as_fd(), 3), (stderr.as_fd(), 2)],
        true,
    );
    if let Err(e) = spawned {
        return failed(&format!("{}: {e}", broker.display()));
    }
    drop(theirs);
    // Kept open through the exec, for the VM.
    let fd = ours.into_raw_fd();
    // SAFETY: fcntl(2) clearing close-on-exec on a descriptor this process owns.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return failed(&format!(
            "the VM's grants socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    // App Sandbox starts the VM in its container: relative paths are of this directory.
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => return failed(&format!("the working directory: {e}")),
    };
    let mut with: Vec<OsString> = Vec::with_capacity(args.len() + 4);
    with.extend(args.first().cloned());
    with.push("--grants".into());
    with.push(fd.to_string().into());
    with.push("--cwd".into());
    with.push(cwd.into());
    with.extend(args.iter().skip(1).cloned());
    instead(&with)
}

/// `shards run --kernel` and `shards restore`: become shards-vm.
#[cfg(not(target_os = "macos"))]
fn vm(args: &[OsString]) -> ExitCode {
    instead(args)
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}");
    ExitCode::FAILURE
}
