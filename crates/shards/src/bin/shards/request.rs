//! `shards run [OPTIONS] IMAGE [COMMAND] [ARG...]`: runs a command in a new microVM booted
//! into an image, as `docker run` runs one in a new container (docs/design/architecture.md
//! D16, D24–D27). The command line, read as the Docker CLI reads it (shards_cmdline),
//! becomes a request (`shards_ipc::Run`), completed with what only this process knows, for
//! the daemon to serve (client.rs). SHARDS_KERNEL and SHARDS_INIT boot those instead of
//! the guest `shards guest use` recorded; only the recorded guest's runs are kept as
//! templates.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(unix)]
use shards_ipc::Identity;
use shards_ipc::{Pull, Run};

use shards_cmdline::commands::RUN;
use shards_cmdline::flags::{Flag, Parsed};

use crate::NOT_RUN;

/// Runs the command line `args`, the words after `path` (`shards run`).
pub fn run(path: &str, args: &[OsString]) -> ExitCode {
    let argv = match crate::utf8(args) {
        Ok(argv) => argv,
        Err(e) => return crate::failed(&e),
    };
    let parsed = match crate::read(&RUN, path, &argv, &validate_env) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
    let mut request = match request(&parsed) {
        Ok(request) => request,
        Err(e) => {
            // As the CLI words its own objections (docker/cli run.go withHelp).
            let _ = writeln!(
                std::io::stderr(),
                "shards: {e}\n\nRun 'shards run --help' for more information"
            );
            return ExitCode::from(NOT_RUN);
        }
    };
    match resolve(&mut request) {
        #[cfg(unix)]
        Ok((home, daemon)) => crate::client::run(&home, &daemon, &request),
        #[cfg(not(unix))]
        Ok(_) => crate::failed(
            "running a command needs the daemon, which needs Unix sockets, which shards does not support on this platform yet",
        ),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}");
            ExitCode::from(NOT_RUN)
        }
    }
}

/// `-e`'s values as the CLI's `opts.ValidateEnv` takes them: `NAME=VALUE` as given, and
/// `NAME` alone with its value here, if it has one (docker/cli opts/env.go).
fn validate_env(flag: &Flag, value: &str) -> Result<String, String> {
    if flag.name != "env" {
        return Ok(value.to_string());
    }
    let (name, given) = match value.split_once('=') {
        Some((name, _)) => (name, true),
        None => (value, false),
    };
    if name.is_empty() {
        return Err(format!("invalid environment variable: {value}"));
    }
    if given {
        return Ok(value.to_string());
    }
    match std::env::var(name) {
        Ok(here) => Ok(format!("{name}={here}")),
        Err(std::env::VarError::NotPresent) => Ok(value.to_string()),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("environment variable {name} is not valid UTF-8"))
        }
    }
}

/// The request `parsed` asks for, or what the CLI objects to before it asks the daemon
/// (docker/cli run.go runRun, validatePullOpt).
fn request(parsed: &Parsed) -> Result<Run, String> {
    let pull = match parsed.string("pull") {
        "missing" | "" => Pull::Missing,
        "always" => Pull::Always,
        "never" => Pull::Never,
        other => {
            return Err(format!(
                "invalid pull option: '{other}': must be one of \"always\", \"missing\" or \"never\""
            ));
        }
    };
    let (image, cmd) = parsed.args.split_first().ok_or("an image is required")?;
    let given = |name: &str| Some(parsed.string(name).to_string()).filter(|v| !v.is_empty());
    Ok(Run {
        image: image.clone(),
        cmd: cmd.to_vec(),
        env: parsed.many("env").to_vec(),
        workdir: parsed.string("workdir").to_string(),
        user: parsed.string("user").to_string(),
        hostname: given("hostname"),
        interactive: parsed.bool("interactive"),
        // Given, the entrypoint is one word, and "" clears the image's (docker/cli
        // cli/command/container/opts.go).
        entrypoint: parsed
            .changed("entrypoint")
            .then(|| given("entrypoint").into_iter().collect()),
        pull,
        name: given("name"),
        detach: parsed.bool("detach"),
        remove: parsed.bool("rm"),
        ..Run::default()
    })
}

/// Completes the request with what only this process knows:
/// - SHARDS_KERNEL and SHARDS_INIT, made absolute;
/// - SHARDS_TIMING;
/// - the daemon binary it would start: `shardsd`, beside this one.
///
/// Returns the home and that binary.
fn resolve(request: &mut Run) -> Result<(PathBuf, PathBuf), String> {
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
    request.kernel = from_env("SHARDS_KERNEL").map(absolute).transpose()?;
    request.init = from_env("SHARDS_INIT").map(absolute).transpose()?;
    if request.kernel.is_some() != request.init.is_some() {
        return Err("SHARDS_KERNEL and SHARDS_INIT go together".into());
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use shards_cmdline::flags::{self, Outcome};

    fn asked(argv: &[&str]) -> Result<Run, String> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        match flags::parse(&RUN, "shards run", &argv, &validate_env) {
            Outcome::Run(parsed) => request(&parsed),
            Outcome::Fail { text, .. } => Err(text),
            Outcome::Help { .. } => Err("help".into()),
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_command_line_becomes_a_request() {
        let asked = asked(&["-e", "A=1", "--entrypoint", "", "--rm", "alpine", "ls", "-e", "/"]).unwrap();
        assert_eq!(asked.image, "alpine");
        assert_eq!(asked.env, strings(&["A=1"]));
        assert_eq!(asked.entrypoint, Some(Vec::new()));
        assert_eq!(
            asked.cmd,
            strings(&["ls", "-e", "/"]),
            "what follows the image is the command's"
        );
        assert!(asked.remove);
        let combined = self::asked(&["-di", "-eA=1", "--name=web", "--pull=never", "alpine"]).unwrap();
        assert!(combined.detach && combined.interactive);
        assert_eq!(
            (combined.name.as_deref(), combined.pull),
            (Some("web"), Pull::Never)
        );
        let given = self::asked(&["-h", "box", "--entrypoint", "/bin/sh", "alpine"]).unwrap();
        assert_eq!(given.hostname.as_deref(), Some("box"));
        assert_eq!(given.entrypoint, Some(strings(&["/bin/sh"])));
        assert_eq!(
            self::asked(&["--pull", "sometimes", "alpine"]).unwrap_err(),
            "invalid pull option: 'sometimes': must be one of \"always\", \"missing\" or \"never\""
        );
        assert!(
            self::asked(&["-t", "alpine"])
                .unwrap_err()
                .contains("\"--tty\" is not supported by shards yet"),
            "no TTYs yet"
        );
    }
}
