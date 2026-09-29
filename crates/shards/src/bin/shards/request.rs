//! `shards run IMAGE [COMMAND] [ARG...]`: runs a command in a new microVM booted into an
//! image, as `docker run` runs one in a new container (docs/design/architecture.md D16,
//! D24–D26). The command line becomes a request (`shards_ipc::Run`), completed with what
//! only this process knows, for the daemon to serve (client.rs).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(unix)]
use shards_ipc::Identity;
use shards_ipc::{Pull, Run};

use crate::NOT_RUN;

const USAGE: &str = "usage: shards run [OPTIONS] IMAGE [COMMAND] [ARG...]
  Runs COMMAND in a new microVM booted into IMAGE, as `docker run` runs it in a new
  container. The image's entrypoint, command, environment, working directory and user
  apply unless given here. IMAGE is pulled first if it is not here.
  Options, as for `docker run`: -e NAME[=VALUE], -w DIR, -u USER[:GROUP], --hostname NAME,
  -i, --entrypoint COMMAND, --pull missing|always|never, --rm (nothing outlives a run).
  --kernel FILE, --init FILE: boot these, instead of the guest `shards guest use` recorded;
  or SHARDS_KERNEL and SHARDS_INIT. Only the recorded guest's runs are kept as templates.";

pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut request = match parse(args) {
        Ok(request) => request,
        Err(e) if e.is_empty() => {
            let _ = writeln!(std::io::stdout(), "{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}\n{USAGE}");
            return ExitCode::from(NOT_RUN);
        }
    };
    match resolve(&mut request) {
        #[cfg(unix)]
        Ok((home, daemon)) => crate::client::run(&home, &daemon, &request),
        #[cfg(not(unix))]
        Ok(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "shards: running a command needs the daemon, which needs Unix sockets, which shards does not support on this platform yet"
            );
            ExitCode::from(NOT_RUN)
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}");
            ExitCode::from(NOT_RUN)
        }
    }
}

/// Completes the request with what only this process knows:
/// - `-e NAME` takes NAME's value here, as the Docker CLI's `ValidateEnv` does (docker/cli
///   opts/env.go); unset here, NAME stays unset in the command;
/// - `--kernel` and `--init`, or SHARDS_KERNEL and SHARDS_INIT, made absolute;
/// - SHARDS_TIMING;
/// - the daemon binary it would start: `shardsd`, beside this one.
///
/// Returns the home and that binary.
fn resolve(request: &mut Run) -> Result<(PathBuf, PathBuf), String> {
    for entry in &mut request.env {
        if entry.contains('=') {
            continue;
        }
        match std::env::var(entry.as_str()) {
            Ok(value) => *entry = format!("{entry}={value}"),
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(format!("environment variable {entry} is not valid UTF-8"));
            }
        }
    }
    let from_env = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let absolute = |path: PathBuf| -> Result<String, String> {
        let full = std::path::absolute(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        full.into_os_string()
            .into_string()
            .map_err(|p| format!("{p:?} is not valid UTF-8"))
    };
    let kernel = request
        .kernel
        .take()
        .map(PathBuf::from)
        .or_else(|| from_env("SHARDS_KERNEL"));
    let init = request
        .init
        .take()
        .map(PathBuf::from)
        .or_else(|| from_env("SHARDS_INIT"));
    request.kernel = kernel.map(absolute).transpose()?;
    request.init = init.map(absolute).transpose()?;
    if request.kernel.is_some() != request.init.is_some() {
        return Err("--kernel and --init (or SHARDS_KERNEL and SHARDS_INIT) go together".into());
    }
    request.timing = std::env::var_os("SHARDS_TIMING").is_some();
    let daemon = crate::shardsd()?;
    // Only Unix has the daemon, so far.
    #[cfg(unix)]
    {
        request.daemon = Identity::of(&daemon).map_err(|e| format!("{}: {e}", daemon.display()))?;
    }
    Ok((shards_ipc::home()?, daemon))
}

/// Options come before the image, as `docker run` takes them; what follows the image is
/// the command.
fn parse(args: impl Iterator<Item = OsString>) -> Result<Run, String> {
    let mut args = args.map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
    });
    let mut asked = Run::default();
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"))?;
        match arg.as_str() {
            "-e" | "--env" => asked.env.push(value("--env")?),
            "-w" | "--workdir" => asked.workdir = value("--workdir")?,
            "-u" | "--user" => asked.user = value("--user")?,
            "--hostname" => asked.hostname = Some(value("--hostname")?),
            "-i" | "--interactive" => asked.interactive = true,
            // docker/cli: a given entrypoint is one word; an empty one clears the image's.
            "--entrypoint" => {
                let e = value("--entrypoint")?;
                asked.entrypoint = Some(if e.is_empty() { Vec::new() } else { vec![e] });
            }
            "--pull" => {
                asked.pull = match value("--pull")?.as_str() {
                    "missing" => Pull::Missing,
                    "always" => Pull::Always,
                    "never" => Pull::Never,
                    other => return Err(format!("--pull: {other:?} is not missing, always or never")),
                }
            }
            "--rm" => {}
            "--kernel" => asked.kernel = Some(value("--kernel")?),
            "--init" => asked.init = Some(value("--init")?),
            "-h" | "--help" => return Err(String::new()),
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag:?}")),
            _ => {
                asked.image = arg;
                asked.cmd = args.by_ref().collect::<Result<_, _>>()?;
                return Ok(asked);
            }
        }
    }
    Err("an image is required".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_come_before_the_image() {
        let args = ["-e", "A=1", "--entrypoint", "", "--rm", "alpine", "ls", "-e", "/"];
        let asked = parse(args.iter().map(OsString::from)).unwrap();
        assert_eq!(asked.image, "alpine");
        assert_eq!(asked.env, strings(&["A=1"]));
        assert_eq!(asked.entrypoint, Some(Vec::new()));
        assert_eq!(
            asked.cmd,
            strings(&["ls", "-e", "/"]),
            "what follows the image is the command's"
        );
        assert!(parse(["--pull", "sometimes", "alpine"].iter().map(OsString::from)).is_err());
        assert!(
            parse(["-t", "alpine"].iter().map(OsString::from)).is_err(),
            "no TTYs yet"
        );
    }
}
