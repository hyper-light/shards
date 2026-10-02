//! `shards`, the command. It reads `run` and the container commands (`ps`, `wait`, `logs`,
//! `rm`, `stop`, `kill`, and each under `container`) as the Docker CLI reads them, answers
//! their `--help` and usage mistakes itself, and asks the daemon for the rest; `shards
//! daemon stop` stops the daemon; `shards vm` is shards-vm's, and every other command
//! shardsd's, each of which runs in this process's place. This binary links only the standard library, `shards_ipc` and
//! `shards_cmdline`, so it starts in a fraction of the time `shardsd` needs, whose
//! frameworks load at every launch (docs/research/platform-measurements.md M23).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use shards_cmdline::flags::{self, Command, Outcome, Parsed};

#[cfg(unix)]
mod client;
mod request;
#[cfg(unix)]
mod save;
#[cfg(unix)]
mod terminal;

/// `docker run`'s status when it could not run the command at all.
const NOT_RUN: u8 = 125;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let words: Vec<&str> = args.iter().map_while(|a| a.to_str()).take(2).collect();
    match words.as_slice() {
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
        ["vm", ..] => vm(args.get(1..).unwrap_or_default()),
        // `build` (or `builder build`, `image build`, `buildx build`, `buildx b`): shardsd's.
        _ if let Some(named) = shards_cmdline::commands::build(&words) => {
            let mut rest = vec![OsString::from("build")];
            rest.extend(args.get(named..).unwrap_or_default().iter().cloned());
            instead(SHARDSD, &rest)
        }
        _ => match shards_cmdline::commands::find(&words) {
            Some((command, path, named)) => container(command, path, &words, named, &args),
            None => instead(SHARDSD, &args),
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
                let fds: Vec<std::os::fd::BorrowedFd<'_>> = output.iter().map(save::Output::fd).collect();
                let status = client::container(
                    &home,
                    &daemon,
                    &shards_ipc::Command {
                        argv,
                        east_asian: shards_cmdline::width::east_asian(|name| {
                            std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
                        }),
                        now: now_ns(),
                        utc_offset: utc_offset(),
                        // SAFETY: isatty(3) on this process's stdout.
                        terminal: unsafe { libc::isatty(1) } == 1,
                        width: terminal::size(1).1,
                        // docker/cli's tui.NewOutput: any NO_COLOR but an empty one.
                        color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
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
        let _ = (words, parsed);
        failed(
            "container commands need the daemon, which needs Unix sockets, which shards does not support on this platform yet",
        )
    }
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
            let help = flags::help(command, path, columns());
            let _ = write!(std::io::stdout(), "{notices}{help}");
            Err(ExitCode::SUCCESS)
        }
        Outcome::Fail {
            notices,
            text,
            status,
        } => {
            let _ = std::io::stdout().write_all(notices.as_bytes());
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

const SHARDSD: &str = "shardsd";
const SHARDS_VM: &str = "shards-vm";

/// The binary `name` beside this one.
fn beside(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    Ok(exe.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

/// `shardsd`, beside this binary.
fn shardsd() -> Result<PathBuf, String> {
    beside(SHARDSD)
}

/// Runs the binary `name` beside this one with `args`, in this process's place: the same
/// pid, stdio and signals.
#[cfg(unix)]
fn instead(name: &str, args: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let bin = match beside(name) {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    let e = std::process::Command::new(&bin).args(args).exec();
    failed(&format!("{}: {e}", bin.display()))
}

/// Runs the binary `name` beside this one with `args`, and exits as it does: Windows has
/// no exec.
#[cfg(not(unix))]
fn instead(name: &str, args: &[OsString]) -> ExitCode {
    let bin = match beside(name) {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    match std::process::Command::new(&bin).args(args).status() {
        Ok(status) => ExitCode::from(status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1)),
        Err(e) => failed(&format!("{}: {e}", bin.display())),
    }
}

/// `shards vm`: becomes shards-vm. On macOS, where shards-vm runs in App Sandbox and may
/// open nothing it is not granted, it first starts the VM's broker, `shardsd grants`, on a
/// socket the VM then asks on (`--grants`; docs/research/macos-confinement.md §3). The VM
/// keeps this process: its terminal, its signals, its exit status. The broker leads a
/// session of its own, out of reach of a Ctrl-C meant for the VM, exits once the VM has
/// all it needs, and is reaped by the kernel, since SIGCHLD stays ignored through the exec.
#[cfg(target_os = "macos")]
fn vm(args: &[OsString]) -> ExitCode {
    use std::os::fd::{AsFd as _, IntoRawFd as _};
    if !matches!(args.first().and_then(|a| a.to_str()), Some("run" | "restore")) {
        return instead(SHARDS_VM, args);
    }
    let broker = match beside(SHARDSD) {
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
    instead(SHARDS_VM, &with)
}

/// `shards vm`: becomes shards-vm.
#[cfg(not(target_os = "macos"))]
fn vm(args: &[OsString]) -> ExitCode {
    instead(SHARDS_VM, args)
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}");
    ExitCode::FAILURE
}
