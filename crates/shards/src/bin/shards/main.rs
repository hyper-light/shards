//! `shards`, the command. `shards run` asks the daemon for a run itself, and `shards daemon
//! stop` stops the daemon; every other command is `shardsd`'s, which runs in this process's
//! place. This binary links only the standard library and `shards_ipc`, so it starts in a
//! fraction of the time `shardsd` needs, whose frameworks load at every launch
//! (docs/research/platform-measurements.md M23).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(unix)]
mod client;
mod request;

/// `docker run`'s status when it could not run the command at all.
const NOT_RUN: u8 = 125;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let word = |i: usize| args.get(i).and_then(|a| a.to_str());
    match (word(0), word(1), args.len()) {
        (Some("run"), _, _) => request::run(args.into_iter().skip(1)),
        #[cfg(unix)]
        (Some("daemon"), Some("stop"), 2) => match shards_ipc::home() {
            Ok(home) => client::stop(&home),
            Err(e) => failed(&e),
        },
        #[cfg(unix)]
        (Some("ps" | "wait" | "rm" | "stop" | "kill" | "logs"), _, _) => container(&args),
        _ => shardsd_instead(&args),
    }
}

/// A container command, for the daemon to run.
#[cfg(unix)]
fn container(args: &[OsString]) -> ExitCode {
    let argv: Result<Vec<String>, String> = args
        .iter()
        .map(|a| {
            a.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("argument {a:?} is not valid UTF-8"))
        })
        .collect();
    let resolved = argv.and_then(|argv| {
        let daemon = shardsd()?;
        let identity = shards_ipc::Identity::of(&daemon).map_err(|e| format!("{}: {e}", daemon.display()))?;
        Ok((argv, daemon, identity, shards_ipc::home()?))
    });
    match resolved {
        Ok((argv, daemon, identity, home)) => client::container(
            &home,
            &daemon,
            &shards_ipc::Command {
                argv,
                daemon: identity,
            },
        ),
        Err(e) => failed(&e),
    }
}

/// `shardsd`, beside this binary.
fn shardsd() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    Ok(exe.with_file_name(format!("shardsd{}", std::env::consts::EXE_SUFFIX)))
}

/// Runs `shardsd ARGS` in this process's place: the same pid, stdio and signals.
#[cfg(unix)]
fn shardsd_instead(args: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let bin = match shardsd() {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    let e = std::process::Command::new(&bin).args(args).exec();
    failed(&format!("{}: {e}", bin.display()))
}

/// Runs `shardsd ARGS` and exits as it does: Windows has no exec.
#[cfg(not(unix))]
fn shardsd_instead(args: &[OsString]) -> ExitCode {
    let bin = match shardsd() {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    match std::process::Command::new(&bin).args(args).status() {
        Ok(status) => ExitCode::from(status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1)),
        Err(e) => failed(&format!("{}: {e}", bin.display())),
    }
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}");
    ExitCode::FAILURE
}
