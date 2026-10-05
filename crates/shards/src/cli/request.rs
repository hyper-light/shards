//! `shards run [OPTIONS] IMAGE [COMMAND] [ARG...]`: runs a command in a new microVM booted
//! into an image, as `docker run` runs one in a new container (docs/design/architecture.md
//! D16, D24–D27). The command line, read as the Docker CLI reads it (shards_cmdline),
//! becomes a request (`shards_ipc::Run`), completed with what only this process knows, for
//! the daemon to serve (client.rs). SHARDS_KERNEL and SHARDS_INIT boot those instead of
//! the guest `shards guest use` recorded; only the recorded guest's runs are kept as
//! templates.

use std::ffi::OsString;
use std::io::{IsTerminal as _, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(unix)]
use shards_ipc::Identity;
use shards_ipc::{Endpoint, Pull, Run};

use shards_cmdline::commands::RUN;
use shards_cmdline::flags::{Flag, Parsed};
use shards_cmdline::network;
use shards_cmdline::term;

use crate::cli::NOT_RUN;

/// Runs the command line `args`, the words after `path` (`shards run`).
pub fn run(path: &str, args: &[OsString]) -> ExitCode {
    let argv = match crate::cli::utf8(args) {
        Ok(argv) => argv,
        Err(e) => return crate::cli::failed(&e),
    };
    let parsed = match crate::cli::read(&RUN, path, &argv, &validate) {
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
    // Checked before anything is created, in the CLI's order, and said as plain errors:
    // exit 1, no prefix (docker/cli run.go runContainer, streams/in.go CheckTty).
    if request.tty.is_some() && request.interactive && !request.detach && !std::io::stdin().is_terminal() {
        return refuse("cannot attach stdin to a TTY-enabled container because stdin is not a terminal");
    }
    let keys = parsed.string("detach-keys");
    let detach_keys = if keys.is_empty() {
        term::DETACH_KEYS.to_vec()
    } else {
        match term::to_bytes(keys) {
            Ok(bytes) => bytes,
            Err(e) => return refuse(&format!("invalid detach keys ({keys}): {e}")),
        }
    };
    match resolve(&mut request) {
        #[cfg(unix)]
        Ok((home, daemon)) => crate::cli::client::run(&home, &daemon, &request, &detach_keys),
        #[cfg(not(unix))]
        Ok(_) => {
            let _ = detach_keys;
            crate::cli::failed(
                "running a command needs the daemon, which needs Unix sockets, which shards does not support on this platform yet",
            )
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}");
            ExitCode::from(NOT_RUN)
        }
    }
}

/// Makes a container as the command line `args` says, the words after `path` (`shards
/// create`), as `docker create` makes one: as `run` would, and not started; its ID said.
pub fn create(path: &str, args: &[OsString]) -> ExitCode {
    let argv = match crate::cli::utf8(args) {
        Ok(argv) => argv,
        Err(e) => return crate::cli::failed(&e),
    };
    let parsed = match crate::cli::read(&shards_cmdline::commands::CREATE, path, &argv, &validate) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
    let mut request = match request(&parsed) {
        Ok(request) => request,
        Err(e) => {
            let _ = writeln!(
                std::io::stderr(),
                "shards: {e}\n\nRun 'shards create --help' for more information"
            );
            return ExitCode::from(NOT_RUN);
        }
    };
    request.create = true;
    request.detach = true;
    send(&mut request, term::DETACH_KEYS)
}

/// Starts the containers the command line `args` names, the words after `path` (`shards
/// start`), as `docker start` does (docker/cli container/start.go): each run again as it
/// was made, its files as it left them (D37), named once it is; with `-a` or `-i`, one,
/// attached.
pub fn start(path: &str, args: &[OsString]) -> ExitCode {
    let argv = match crate::cli::utf8(args) {
        Ok(argv) => argv,
        Err(e) => return crate::cli::failed(&e),
    };
    let parsed = match crate::cli::read(&shards_cmdline::commands::START, path, &argv, &validate) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
    let keys = parsed.string("detach-keys");
    let detach_keys = if keys.is_empty() {
        term::DETACH_KEYS.to_vec()
    } else {
        match term::to_bytes(keys) {
            Ok(bytes) => bytes,
            Err(e) => return refuse(&format!("invalid detach keys ({keys}): {e}")),
        }
    };
    let (attach, interactive) = (parsed.bool("attach"), parsed.bool("interactive"));
    if attach || interactive {
        if parsed.args.len() > 1 {
            return refuse("you cannot start and attach multiple containers at once");
        }
        let mut request = Run {
            again: parsed.args.first().cloned(),
            interactive,
            tty: std::io::stdout().is_terminal().then(stdout_size),
            ..Run::default()
        };
        return send(&mut request, &detach_keys);
    }
    // startContainersWithoutAttachments: each named as it starts, the others' errors
    // said, and their names after.
    let mut failed = Vec::new();
    for container in &parsed.args {
        let mut request = Run {
            again: Some(container.clone()),
            detach: true,
            ..Run::default()
        };
        if send(&mut request, &detach_keys) != ExitCode::SUCCESS {
            failed.push(container.as_str());
        }
    }
    if failed.is_empty() {
        return ExitCode::SUCCESS;
    }
    refuse(&format!("failed to start containers: {}", failed.join(", ")))
}

/// Restarts the containers the command line `args` names, the words after `path`
/// (`shards restart`), as `docker restart` does (docker/cli container/restart.go): each
/// stopped, by `-s` and `-t` where given, then started again, and named.
pub fn restart(path: &str, args: &[OsString]) -> ExitCode {
    let argv = match crate::cli::utf8(args) {
        Ok(argv) => argv,
        Err(e) => return crate::cli::failed(&e),
    };
    let parsed = match crate::cli::read(&shards_cmdline::commands::RESTART, path, &argv, &validate) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
    if parsed.changed("time") && parsed.changed("timeout") {
        return refuse("conflicting options: cannot specify both --timeout and --time");
    }
    let signal = parsed.string("signal");
    let timeout = (parsed.changed("timeout") || parsed.changed("time")).then(|| parsed.int("timeout"));
    let mut status = ExitCode::SUCCESS;
    for container in &parsed.args {
        let mut request = Run {
            again: Some(container.clone()),
            restart: true,
            detach: true,
            stop_signal: (!signal.is_empty()).then(|| signal.to_string()),
            stop_timeout: timeout,
            ..Run::default()
        };
        if send(&mut request, term::DETACH_KEYS) != ExitCode::SUCCESS {
            status = ExitCode::FAILURE;
        }
    }
    status
}

/// Sends `request` to the daemon as a run, answered as one.
fn send(request: &mut Run, detach_keys: &[u8]) -> ExitCode {
    match resolve(request) {
        #[cfg(unix)]
        Ok((home, daemon)) => crate::cli::client::run(&home, &daemon, request, detach_keys),
        #[cfg(not(unix))]
        Ok(_) => {
            let _ = detach_keys;
            crate::cli::failed(
                "running a command needs the daemon, which needs Unix sockets, which shards does not support on this platform yet",
            )
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}");
            ExitCode::from(NOT_RUN)
        }
    }
}

/// Runs the command line `args`, the words after `path` (`shards exec`), as `docker exec`
/// runs it: in a running container, attached unless `-d` (docker/cli
/// cli/command/container/exec.go).
pub fn exec(path: &str, args: &[OsString]) -> ExitCode {
    let argv = match crate::cli::utf8(args) {
        Ok(argv) => argv,
        Err(e) => return crate::cli::failed(&e),
    };
    let parsed = match crate::cli::read(&shards_cmdline::commands::EXEC, path, &argv, &validate) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
    let Some((container, cmd)) = parsed.args.split_first() else {
        return crate::cli::failed("a container is required");
    };
    let request = shards_ipc::Exec {
        container: container.clone(),
        cmd: cmd.to_vec(),
        env: parsed.many("env").to_vec(),
        user: parsed.string("user").to_string(),
        workdir: parsed.string("workdir").to_string(),
        interactive: parsed.bool("interactive"),
        detach: parsed.bool("detach"),
        tty: parsed.bool("tty").then(stdout_size),
        // `-it` without a terminal is refused by the daemon, once it has found the
        // container: the CLI inspects it first, so that "No such container" comes first
        // (docker/cli exec.go RunExec).
        stdin_terminal: std::io::stdin().is_terminal(),
        ..shards_ipc::Exec::default()
    };
    // Before the container is looked for, as the CLI checks them (exec.go parseExec).
    let keys = parsed.string("detach-keys");
    let detach_keys = if keys.is_empty() {
        term::DETACH_KEYS.to_vec()
    } else {
        match term::to_bytes(keys) {
            Ok(bytes) => bytes,
            Err(e) => return refuse(&format!("invalid detach keys ({keys}): {e}")),
        }
    };
    #[cfg(unix)]
    {
        let daemon = match crate::cli::shardsd() {
            Ok(daemon) => daemon,
            Err(e) => return crate::cli::failed(&e),
        };
        let mut request = request;
        request.daemon = match Identity::of_build(&daemon) {
            Ok(identity) => identity,
            Err(e) => return crate::cli::failed(&format!("{}: {e}", daemon.display())),
        };
        match shards_ipc::home() {
            Ok(home) => crate::cli::client::exec(&home, &daemon, &request, &detach_keys),
            Err(e) => crate::cli::failed(&e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (request, detach_keys);
        crate::cli::failed(
            "running a command needs the daemon, which needs Unix sockets, which shards does not support on this platform yet",
        )
    }
}

/// Says `why` as the CLI says a plain error, and exits 1 (docker/cli cmd/docker/docker.go).
fn refuse(why: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "{why}");
    ExitCode::FAILURE
}

/// Flag values the CLI checks as it reads them: `--network`'s as `NetworkOpt.Set` reads
/// them (docker/cli opts/network.go), and `-e`'s as `opts.ValidateEnv` takes them:
/// `NAME=VALUE` as given, and `NAME` alone with its value here, if it has one
/// (docker/cli opts/env.go).
fn validate(flag: &Flag, value: &str) -> Result<String, String> {
    if matches!(flag.name, "network" | "net") {
        return network::attachment(value).map(|_| value.to_string());
    }
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
    let health = health(parsed)?;
    let (_, bindings) = shards_cmdline::ports::publish(parsed.many("publish"))?;
    let attachments = parsed
        .many("network")
        .iter()
        .map(|v| network::attachment(v))
        .collect::<Result<Vec<_>, _>>()?;
    let endpoints = network::endpoints(&attachments)?
        .into_iter()
        .map(|a| Endpoint {
            network: a.target,
            aliases: a.aliases,
            ipv4: a.ipv4.map(|a| a.to_string()).unwrap_or_default(),
            ipv6: a.ipv6.map(|a| a.to_string()).unwrap_or_default(),
            link_local: a.link_local.iter().map(ToString::to_string).collect(),
            mac: a.mac,
            driver_opts: a.driver_opts,
            gw_priority: a.gw_priority,
        })
        .collect();
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
        // Sized as the CLI's stdout is, even detached (docker/cli create.go, ConsoleSize).
        tty: parsed.bool("tty").then(stdout_size),
        network: network::mode(&attachments).to_string(),
        endpoints,
        stop_signal: parsed
            .changed("stop-signal")
            .then(|| parsed.string("stop-signal").to_string()),
        stop_timeout: parsed.changed("stop-timeout").then(|| parsed.int("stop-timeout")),
        health,
        publish: bindings
            .into_iter()
            .map(|b| shards_ipc::Publish {
                port: b.port.number,
                proto: b.port.proto,
                host_ip: b.host_ip,
                host_port: b.host_port,
            })
            .collect(),
        publish_all: parsed.bool("publish-all"),
        registry_env: shards_ipc::registry_env(),
        ..Run::default()
    })
}

/// The health check the command line sets, as the CLI reads it (docker/cli
/// cli/command/container/opts.go, parse): `NONE` for `--no-healthcheck`, which no other
/// `--health-*` may join; otherwise what the `--health-*` flags say, none negative, and
/// `--health-cmd` as `CMD-SHELL`; or nothing, for the image's.
fn health(parsed: &Parsed) -> Result<Option<shards_ipc::Health>, String> {
    let given = shards_ipc::Health {
        test: match parsed.string("health-cmd") {
            "" => Vec::new(),
            cmd => vec!["CMD-SHELL".into(), cmd.into()],
        },
        interval: parsed.int("health-interval"),
        timeout: parsed.int("health-timeout"),
        start_period: parsed.int("health-start-period"),
        start_interval: parsed.int("health-start-interval"),
        retries: parsed.int("health-retries"),
    };
    let any = given != shards_ipc::Health::default();
    if parsed.bool("no-healthcheck") {
        if any {
            return Err("--no-healthcheck conflicts with --health-* options".into());
        }
        return Ok(Some(shards_ipc::Health {
            test: vec!["NONE".into()],
            ..shards_ipc::Health::default()
        }));
    }
    if !any {
        return Ok(None);
    }
    for (value, flag) in [
        (given.interval, "--health-interval"),
        (given.timeout, "--health-timeout"),
        (given.retries, "--health-retries"),
        (given.start_period, "--health-start-period"),
        (given.start_interval, "--health-start-interval"),
    ] {
        if value < 0 {
            return Err(format!("{flag} cannot be negative"));
        }
    }
    Ok(Some(given))
}

/// Rows and columns of this process's stdout, or 0×0 if it is not a terminal.
fn stdout_size() -> (u16, u16) {
    #[cfg(unix)]
    return crate::cli::terminal::size(1);
    #[cfg(not(unix))]
    (0, 0)
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
    let daemon = crate::cli::shardsd()?;
    // Only Unix has the daemon, so far.
    #[cfg(unix)]
    {
        request.daemon = Identity::of_build(&daemon).map_err(|e| format!("{}: {e}", daemon.display()))?;
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
        match flags::parse(&RUN, "shards run", &argv, &validate) {
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
        let tty = self::asked(&["-t", "alpine"]).unwrap();
        assert!(tty.tty.is_some(), "a terminal, as big as stdout");
        let published = self::asked(&["-p", "127.0.0.1:8080:80/udp", "-P", "alpine"]).unwrap();
        assert_eq!(
            published.publish,
            [shards_ipc::Publish {
                port: 80,
                proto: "udp".into(),
                host_ip: "127.0.0.1".into(),
                host_port: "8080".into(),
            }]
        );
        assert!(published.publish_all);
        assert_eq!(
            self::asked(&["-p", "x", "alpine"]).unwrap_err(),
            "invalid containerPort: x"
        );
    }
}
