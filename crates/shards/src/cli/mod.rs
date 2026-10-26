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
pub(crate) use client::{Stream, ask_here};
#[cfg(unix)]
pub(crate) mod cp;
#[cfg(unix)]
pub(crate) mod listing;
#[cfg(unix)]
pub(crate) mod look;
pub(crate) mod request;
pub(crate) mod save;
#[cfg(unix)]
mod screens;
#[cfg(unix)]
mod show;
pub(crate) mod terminal;
pub(crate) mod terms;
pub(crate) mod version;

/// `docker run`'s status when it could not run the command at all.
const NOT_RUN: u8 = 125;

/// The environment's value of shards' own variable `ours`, else of `theirs`, the name
/// Docker's or buildx's tools read for the same: each set and not empty.
pub(crate) fn env_ours_first(ours: &str, theirs: &str) -> Option<String> {
    [ours, theirs]
        .into_iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
}

pub fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // A context (D180): the command runs whole on its machine. A context's own commands
    // run here, whichever is chosen.
    if let Ok(home) = shards_ipc::home() {
        match crate::context::chosen(&home, &mut args) {
            Ok(crate::context::Chosen::There(context)) if !crate::context::kept_here(&args) => {
                return crate::context::forward(&home, &context, args);
            }
            Err(e) if !crate::context::kept_here(&args) => return failed(&e),
            _ => {}
        }
    }
    dispatch(args)
}

/// The commands shards has besides the catalog's: the daemon side's own words.
/// `daemon`, `guest` and `disk` are what `run daemon`, `stop daemon`, `configure guest` and
/// `configure disk` are said as, and `daemon` how the client starts the daemon.
const OWN: [&str; 26] = [
    "system",
    // Contexts, and the clients and invitations of this machine's (D180).
    "context",
    "client",
    "invitation",
    // A microVM's snapshots (`list snapshots`, `remove snapshot`, D176).
    "snapshot",
    // shards' grammar's views of a microVM (`read ports`, D122; `read` and `watch
    // processes`, D123).
    "read",
    "watch",
    "agentfile",
    "agent",
    "harness",
    "mcp",
    "daemon",
    "guest",
    "disk",
    "grants",
    "working-set",
    "share",
    "frontend",
    "spawner",
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
    // An action of shards' own, alone or asked for its help: its page, wherever it is
    // asked. One Docker has a command of the same name for is its page on a colour
    // terminal alone (below).
    if let [action] | [action, _] = text.as_slice()
        && shards_cmdline::grammar::is_action(action)
        && shards_cmdline::commands::find(&[action.as_str()]).is_none()
        && text.get(1).is_none_or(|h| h == "-h" || h == "--help")
    {
        return action_help(action);
    }
    // An action Docker has too, alone, on a colour terminal: its page, where Docker's
    // answer is that it lacks an argument; elsewhere, Docker's answer.
    #[cfg(unix)]
    if let [action] = text.as_slice()
        && shards_cmdline::grammar::is_action(action)
        && !shards_cmdline::commands::find(&[action.as_str()]).is_some_and(|(c, _, _)| {
            matches!(
                c.args,
                shards_cmdline::flags::Args::Any
                    | shards_cmdline::flags::Args::None
                    | shards_cmdline::flags::Args::AtMost(_)
            )
        })
        && look::styled_quiet()
    {
        return action_help(action);
    }
    if text.len() == args.len()
        && let Some(said) = shards_cmdline::grammar::rewrite(&text)
        && said != text
    {
        // The page is named as it was asked for: `list vms`, not `ps`; and is that form's,
        // in shards' words, where help is asked.
        #[cfg(unix)]
        look::name_as(&text.iter().take(2).cloned().collect::<Vec<_>>().join(" "));
        if let Some(form) = shards_cmdline::grammar::form(&text) {
            let _ = FORM.set(form);
        }
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
        [name @ ("image" | "container" | "volume" | "checkpoint")]
        | [
            name @ ("image" | "container" | "volume" | "checkpoint"),
            "-h" | "--help",
        ] => {
            return management_help(name);
        }
        [name @ ("image" | "container" | "volume" | "checkpoint"), word, ..]
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
        ["context", ..] => crate::context::command(args.get(1..).unwrap_or_default()),
        [thing @ ("client" | "invitation"), ..] => {
            crate::context::admin(thing, args.get(1..).unwrap_or_default())
        }
        ["container", "run", ..] => request::run("shards container run", args.get(2..).unwrap_or_default()),
        ["exec", ..] => request::exec("shards exec", args.get(1..).unwrap_or_default()),
        ["attach", ..] => request::attach("shards attach", args.get(1..).unwrap_or_default()),
        ["container", "attach", ..] => {
            request::attach("shards container attach", args.get(2..).unwrap_or_default())
        }
        ["create", ..] => request::create("shards create", args.get(1..).unwrap_or_default()),
        ["container", "create", ..] => {
            request::create("shards container create", args.get(2..).unwrap_or_default())
        }
        ["start", ..] => request::start("shards start", args.get(1..).unwrap_or_default()),
        ["container", "start", ..] => {
            request::start("shards container start", args.get(2..).unwrap_or_default())
        }
        ["restart", ..] => request::restart("shards restart", args.get(1..).unwrap_or_default()),
        ["container", "restart", ..] => {
            request::restart("shards container restart", args.get(2..).unwrap_or_default())
        }
        ["container", "exec", ..] => {
            request::exec("shards container exec", args.get(2..).unwrap_or_default())
        }
        #[cfg(unix)]
        ["daemon", "stop"] if args.len() == 2 => match shards_ipc::home() {
            Ok(home) => client::stop(&home),
            Err(e) => failed(&e),
        },
        // `shards restore DIR`: a microVM resumed from the snapshot a directory holds, in
        // the VM process; anything else names a stopped microVM, which its daemon resumes
        // from a snapshot of its own (D176).
        ["restore", target, ..] if names_a_microvm(target) => {
            request::restore("shards restore vm", args.get(1..).unwrap_or_default())
        }
        ["restore", ..] => vm(&args),
        // This shards' own version: nothing asked of a daemon (version.rs).
        ["version", ..] => version::run(args.get(1..).unwrap_or_default()),
        // `buildx policy ...`: buildx's policy commands, run here as a build is (D108).
        ["buildx", "policy"] => {
            let mut rest = vec![OsString::from("policy")];
            rest.extend(args.get(2..).unwrap_or_default().iter().cloned());
            crate::shardsd(rest)
        }
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

/// Whether `restore`'s first argument names a microVM, which its daemon resumes from a
/// snapshot of its own (D176), rather than a directory holding a snapshot: a name a
/// microVM may have, and so no path, and no directory of that name here holding a
/// snapshot's pointer, damaged or not. A path is always a directory's, whose restore says
/// what is wrong with it.
fn names_a_microvm(target: &str) -> bool {
    // As a container's name is (moby daemon/names/names.go): `[a-zA-Z0-9][a-zA-Z0-9_.-]+`.
    let name = target.as_bytes();
    let named = name.len() > 1
        && name.first().is_some_and(u8::is_ascii_alphanumeric)
        && name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    named && !shards_vmm::snapshot::pointer(std::path::Path::new(target)).exists()
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
    let mut argv = match utf8(args.get(named..).unwrap_or_default()) {
        Ok(argv) => argv,
        Err(e) => return failed(&e),
    };
    // `pull`'s platform is SHARDS_DEFAULT_PLATFORM's, else DOCKER_DEFAULT_PLATFORM's, where
    // the command line names none (docker/cli v29.8.1 cli/command/image/pull.go): said
    // first, so that one named after it is the one kept, as pflag keeps the last given.
    if std::ptr::eq(command, &shards_cmdline::commands::PULL)
        && let Some(p) = env_ours_first("SHARDS_DEFAULT_PLATFORM", "DOCKER_DEFAULT_PLATFORM")
    {
        argv.splice(0..0, ["--platform".to_string(), p]);
    }
    let parsed = match read(command, path, &argv, &flags::value) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
    // `pull --output-agentfile`: the image's Agentfile as well as its microVM (AGENTFILE_ARCH.md
    // §10), its file made before the pull, which fills it once it is through.
    let agentfile = match pulls_agentfile(command, &parsed) {
        Ok(agentfile) => agentfile,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "{e}");
            return ExitCode::FAILURE;
        }
    };
    // A listing's `--format` is the client's to apply, and to check first, as the CLI
    // checks it before it asks (container/list.go, buildContainerListOptions).
    #[cfg(unix)]
    if std::ptr::eq(command, &shards_cmdline::commands::PS) {
        let format = parsed.string("format").to_string();
        if !format.is_empty() {
            let clock = shards_cmdline::format::Clock {
                now: 0,
                zone: &shards_cmdline::format::utc,
            };
            if let Err(e) = shards_cmdline::format::container::check(&format, &clock) {
                let _ = writeln!(std::io::stderr(), "{e}");
                return ExitCode::FAILURE;
            }
        }
        listing::ask(listing::Asked {
            format,
            quiet: parsed.bool("quiet"),
            trunc: !parsed.bool("no-trunc"),
            size: parsed.bool("size"),
            ..listing::Asked::default()
        });
    }
    #[cfg(unix)]
    if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_LS) {
        listing::ask(listing::Asked {
            format: parsed.string("format").to_string(),
            quiet: parsed.bool("quiet"),
            trunc: !parsed.bool("no-trunc"),
            ..listing::Asked::default()
        });
    }
    #[cfg(unix)]
    if std::ptr::eq(command, &shards_cmdline::commands::IMAGES) {
        listing::ask(listing::Asked {
            format: parsed.string("format").to_string(),
            quiet: parsed.bool("quiet"),
            trunc: !parsed.bool("no-trunc"),
            digests: parsed.bool("digests"),
            human: false,
            verbose: false,
            size: false,
            filtered: false,
        });
    }
    #[cfg(unix)]
    if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_LS) {
        listing::ask(listing::Asked {
            format: parsed.string("format").to_string(),
            quiet: parsed.bool("quiet"),
            ..listing::Asked::default()
        });
    }
    // `system prune --volumes` with `until`: the CLI's own refusal (volume/prune.go,
    // pruneFn), before it asks.
    if std::ptr::eq(command, &shards_cmdline::commands::SYSTEM_PRUNE)
        && parsed.bool("volumes")
        && parsed
            .many("filter")
            .iter()
            .any(|f| f.split_once('=').is_some_and(|(k, _)| k == "until"))
    {
        let _ = writeln!(
            std::io::stderr(),
            "ERROR: The \"until\" filter is not supported with \"--volumes\""
        );
        return ExitCode::FAILURE;
    }
    // `update` with no flag: the CLI's own refusal (container/update.go, NFlag).
    if std::ptr::eq(command, &shards_cmdline::commands::UPDATE)
        && !command.flags.iter().any(|f| parsed.changed(f.name))
        && !["cpu-rt-period", "cpu-rt-runtime"]
            .iter()
            .any(|f| parsed.changed(f))
    {
        let _ = writeln!(
            std::io::stderr(),
            "you must provide one or more flags when using this command"
        );
        return ExitCode::FAILURE;
    }
    // `volume prune --all` and a filter `all` both, refused as the CLI refuses them
    // (volume/prune.go), in shards' words.
    if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_PRUNE)
        && parsed.bool("all")
        && parsed
            .many("filter")
            .iter()
            .any(|f| f.split_once('=').is_some_and(|(k, _)| k == "all"))
    {
        let _ = writeln!(
            std::io::stderr(),
            "--all and --filter all=1 each prune every unused volume: give one"
        );
        return ExitCode::FAILURE;
    }
    #[cfg(unix)]
    if std::ptr::eq(command, &shards_cmdline::commands::SYSTEM_DF) {
        listing::ask(listing::Asked {
            format: parsed.string("format").to_string(),
            verbose: parsed.bool("verbose"),
            ..listing::Asked::default()
        });
    }
    // `buildx du`: what runDiskUsage refuses before it asks (diskusage.go), as buildx's
    // main prints it.
    if std::ptr::eq(command, &shards_cmdline::commands::BUILDER_DU) {
        let asked = shards_cmdline::format::du::format(parsed.string("format"), parsed.bool("verbose"))
            .and_then(|_| shards_cmdline::buildcache::prune_info(parsed.many("filter")));
        let filtered = match asked {
            Ok(info) => info.given,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "ERROR: {e}");
                return ExitCode::FAILURE;
            }
        };
        #[cfg(unix)]
        listing::ask(listing::Asked {
            format: parsed.string("format").to_string(),
            verbose: parsed.bool("verbose"),
            filtered,
            ..listing::Asked::default()
        });
        #[cfg(not(unix))]
        let _ = filtered;
    }
    #[cfg(unix)]
    if std::ptr::eq(command, &shards_cmdline::commands::HISTORY) {
        listing::ask(listing::Asked {
            format: parsed.string("format").to_string(),
            quiet: parsed.bool("quiet"),
            trunc: !parsed.bool("no-trunc"),
            digests: false,
            human: parsed.bool("human"),
            verbose: false,
            size: false,
            filtered: false,
        });
    }
    // A prune asks first, as the Docker CLI does, unless forced: on a colour terminal in
    // shards' look, elsewhere in the CLI's words.
    if let Some(warning) = prune_warning(command, &parsed) {
        #[cfg(unix)]
        let asked = match look::styled() {
            Some(p) => {
                let items = prune_items(command, &parsed);
                let items: Vec<&str> = items.iter().map(String::as_str).collect();
                look::confirm(&p, &look_name(path), &items)
            }
            None => confirmed(&warning),
        };
        #[cfg(not(unix))]
        let asked = confirmed(&warning);
        if !asked {
            return ExitCode::SUCCESS;
        }
    }
    #[cfg(unix)]
    {
        // `save` and `export` write where the client says, opened here, before the
        // client moves to the daemon's home.
        let written = if std::ptr::eq(command, &shards_cmdline::commands::SAVE) {
            Some("failed to save image")
        } else if std::ptr::eq(command, &shards_cmdline::commands::EXPORT) {
            Some("failed to export container")
        } else {
            None
        };
        let output = if let Some(written) = written {
            match save::output(parsed.string("output"), written) {
                Ok(output) => Some(output),
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            None
        };
        // `import` reads the file the client opens, or its stdin (`-`); a URL is the
        // daemon's to fetch (import.go runImport).
        let mut argv = argv;
        let input = if std::ptr::eq(command, &shards_cmdline::commands::IMPORT) {
            // The flag's default is SHARDS_DEFAULT_PLATFORM's, else DOCKER_DEFAULT_PLATFORM's.
            if !parsed.changed("platform")
                && let Some(p) = env_ours_first("SHARDS_DEFAULT_PLATFORM", "DOCKER_DEFAULT_PLATFORM")
            {
                argv.splice(0..0, ["--platform".to_string(), p]);
            }
            match parsed.args.first().map(String::as_str) {
                Some("-") => Some(None),
                Some(s) if s.starts_with("http://") || s.starts_with("https://") => None,
                Some(path) => match std::fs::File::open(path) {
                    Ok(f) => Some(Some(f)),
                    Err(e) => {
                        let _ = writeln!(std::io::stderr(), "open {path}: {}", save::go(&e));
                        return ExitCode::FAILURE;
                    }
                },
                None => None,
            }
        } else if std::ptr::eq(command, &shards_cmdline::commands::LOAD) {
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
        argv.splice(0..0, words.iter().take(named).map(|w| (*w).to_string()));
        let resolved = shardsd().and_then(|daemon| {
            let identity =
                shards_ipc::Identity::of_build(&daemon).map_err(|e| format!("{}: {e}", daemon.display()))?;
            Ok((daemon, identity, shards_ipc::home()?))
        });
        match resolved {
            Ok((daemon, identity, home)) => {
                use std::os::fd::AsFd as _;
                // What the daemon writes, the command's output; what it reads, its input.
                let mut fds: Vec<client::Stream<'_>> =
                    output.iter().map(|o| client::Stream::To(o.fd())).collect();
                match &input {
                    Some(Some(file)) => fds.push(client::Stream::From(file.as_fd())),
                    Some(None) => fds.push(client::Stream::From(stdin.as_fd())),
                    None => {}
                }
                let command_of = |argv: Vec<String>| shards_ipc::Command {
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
                };
                // `cp` asks in steps, and moves the files itself (cli/cp.rs).
                if std::ptr::eq(command, &shards_cmdline::commands::COPY) {
                    drop(fds);
                    return cp::copy(&parsed, &|argv, fds| {
                        client::ask(&home, &daemon, &command_of(argv), fds)
                    });
                }
                let status = client::container(&home, &daemon, &command_of(argv), &fds);
                drop(fds);
                if let Some(agentfile) = agentfile {
                    return pulled_agentfile(agentfile, status, &home, &parsed);
                }
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
            let status = crate::pull::command(&parsed, &home, &env, &out, None);
            if let Some(agentfile) = agentfile {
                return pulled_agentfile(agentfile, status, &home, &parsed);
            }
            return ExitCode::from(status);
        }
        failed(
            "container commands need the daemon, which needs Unix sockets, which shards does not support on this platform yet",
        )
    }
}

/// The file `pull --output-agentfile` names, ready before the pull ([`crate::from_image::output`]);
/// none for another command, or none named. An Agentfile is one image's, which `-a` does
/// not name.
fn pulls_agentfile(
    command: &'static Command,
    parsed: &flags::Parsed,
) -> Result<Option<save::Output>, String> {
    let named = parsed.string("output-agentfile");
    if !std::ptr::eq(command, &shards_cmdline::commands::PULL) || named.is_empty() {
        return Ok(None);
    }
    if parsed.bool("all-tags") {
        return Err(
            "shards: --output-agentfile writes one image's Agentfile, which --all-tags does not name".into(),
        );
    }
    crate::from_image::output(named).map(Some)
}

/// What a pull that wrote `agentfile` ends with: the Agentfile of the image it pulled, as
/// stored, once it exited `status` 0; else nothing written, and the pull's status.
fn pulled_agentfile(
    agentfile: save::Output,
    status: u8,
    home: &std::path::Path,
    parsed: &flags::Parsed,
) -> ExitCode {
    if status != 0 {
        return ExitCode::from(status);
    }
    let written = (|| {
        let given = parsed.args.first().map(String::as_str).unwrap_or_default();
        let reference =
            shards_image::reference::Reference::parse(given).map_err(|e| format!("{given}: {e}"))?;
        let targets = crate::pull::targets(parsed.string("platform"))?;
        let text = crate::from_image::stored(home, &reference, &targets)?;
        crate::from_image::write_into(agentfile, &text)
    })();
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "{e}");
            ExitCode::FAILURE
        }
    }
}

/// The help of one of shards' own actions: what it does to each thing it takes.
fn action_help(action: &str) -> ExitCode {
    let rows: Vec<(&str, &str)> = shards_cmdline::grammar::USES
        .iter()
        .filter(|(_, a, _, _)| *a == action)
        .map(|(_, _, takes, about)| (*takes, *about))
        .collect();
    #[cfg(unix)]
    if let Some(p) = look::styled() {
        look::action(&p, action, &rows, &mut std::io::stdout().lock());
        return ExitCode::SUCCESS;
    }
    let mut text = format!("Usage:  shards {action} THING [ARG...]\n\n");
    for (takes, about) in rows {
        text.push_str(&format!("  shards {action} {takes:<20} {about}\n"));
    }
    let _ = std::io::stdout().write_all(text.as_bytes());
    ExitCode::SUCCESS
}

/// `shards --help`: shards' grammar, its own page on a colour terminal, plain elsewhere.
fn top_help() -> ExitCode {
    #[cfg(unix)]
    if let Some(p) = look::styled() {
        look::top(&p, &mut std::io::stdout().lock());
        return ExitCode::SUCCESS;
    }
    let page = shards_cmdline::grammar::help(shards_cmdline::catalog::ABOUT);
    let _ = std::io::stdout().write_all(page.as_bytes());
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

/// What a prune warns of before it removes anything, as docker/cli words it
/// (container/prune.go, image/prune.go, system/prune.go); none if it is forced or is no
/// prune.
fn prune_warning(command: &'static Command, parsed: &Parsed) -> Option<String> {
    use shards_cmdline::commands::{CONTAINER_PRUNE, IMAGE_PRUNE, SYSTEM_PRUNE, VOLUME_PRUNE};
    if parsed.bool("force") {
        return None;
    }
    // buildx's prune (commands/prune.go): its warning and promptForConfirmation's ask.
    if std::ptr::eq(command, &shards_cmdline::commands::BUILDER_PRUNE) {
        let which = if parsed.bool("all") { "all" } else { "all dangling" };
        return Some(format!(
            "WARNING! This will remove {which} build cache. Are you sure you want to continue? [y/N] "
        ));
    }
    if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_PRUNE) {
        return Some(
            "WARNING! This will remove all custom networks not used by at least one container.\nAre you sure you want to continue? [y/N] "
                .into(),
        );
    }
    if std::ptr::eq(command, &VOLUME_PRUNE) {
        let which = if parsed.bool("all") { "all" } else { "anonymous" };
        return Some(format!(
            "WARNING! This will remove {which} local volumes not used by at least one container.\nAre you sure you want to continue? [y/N] "
        ));
    }
    let all = std::ptr::eq(command, &IMAGE_PRUNE) || std::ptr::eq(command, &SYSTEM_PRUNE);
    let all = all && parsed.bool("all");
    let ask = "Are you sure you want to continue? [y/N] ";
    if std::ptr::eq(command, &CONTAINER_PRUNE) {
        Some(format!(
            "WARNING! This will remove all stopped containers.\n{ask}"
        ))
    } else if std::ptr::eq(command, &IMAGE_PRUNE) {
        Some(if all {
            format!(
                "WARNING! This will remove all images without at least one container associated to them.\n{ask}"
            )
        } else {
            format!("WARNING! This will remove all dangling images.\n{ask}")
        })
    } else if std::ptr::eq(command, &SYSTEM_PRUNE) {
        let (images, cache) = if all {
            (
                "all images without at least one container associated to them",
                "all build cache",
            )
        } else {
            ("all dangling images", "unused build cache")
        };
        // Volumes after networks (pruner.pruneOrder), with `--volumes`.
        let volumes = if parsed.bool("volumes") {
            "\n  - all anonymous volumes not used by at least one container"
        } else {
            ""
        };
        // confirmationTemplate: the filters, where there are any, below the list.
        let filters: String = prune_filters(parsed)
            .iter()
            .map(|f| format!("\n  - {f}"))
            .collect();
        let filtered = if filters.is_empty() {
            String::new()
        } else {
            format!("\n  Items to be pruned will be filtered with:{filters}\n")
        };
        Some(format!(
            "WARNING! This will remove:\n  - all stopped containers\n  - all networks not used by at least one container{volumes}\n  - {images}\n  - {cache}\n{filtered}\n{ask}"
        ))
    } else {
        None
    }
}

/// A page's name for the command `path` names: `prune image` for `shards image prune`.
#[cfg(unix)]
fn look_name(path: &str) -> String {
    let words: Vec<&str> = path.split(' ').skip(1).collect();
    match words.as_slice() {
        ["container", "prune"] => "prune vm".into(),
        [thing, "prune"] => format!("prune {thing}"),
        _ => words.join(" "),
    }
}

/// What a prune removes, in shards' words, for its page.
#[cfg(unix)]
fn prune_items(command: &'static Command, parsed: &Parsed) -> Vec<String> {
    use shards_cmdline::commands::{CONTAINER_PRUNE, IMAGE_PRUNE, VOLUME_PRUNE};
    if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_PRUNE) {
        let mut items = vec!["every custom network no microVM is on".to_string()];
        items.extend(
            prune_filters(parsed)
                .into_iter()
                .map(|f| format!("only those {f} keeps")),
        );
        return items;
    }
    if std::ptr::eq(command, &VOLUME_PRUNE) {
        let mut items = vec![if parsed.bool("all") {
            "every volume no microVM mounts".to_string()
        } else {
            "every anonymous volume no microVM mounts".to_string()
        }];
        items.extend(
            prune_filters(parsed)
                .into_iter()
                .map(|f| format!("only those {f} keeps")),
        );
        return items;
    }
    let images = if parsed.bool("all") {
        "every image no microVM was made from"
    } else {
        "every dangling image: those no name reaches"
    };
    let mut items: Vec<String> = if std::ptr::eq(command, &CONTAINER_PRUNE) {
        vec!["every stopped microVM".into()]
    } else if std::ptr::eq(command, &IMAGE_PRUNE) {
        vec![images.into()]
    } else {
        vec!["every stopped microVM".into(), images.into()]
    };
    items.extend(
        prune_filters(parsed)
            .into_iter()
            .map(|f| format!("only those {f} keeps")),
    );
    items
}

/// The filters a prune asks with, as the CLI lists them (system/prune.go,
/// confirmationMessage): `label`'s, `label!`'s and `until`'s, in natural order.
fn prune_filters(parsed: &Parsed) -> Vec<String> {
    let mut filters: Vec<String> = Vec::new();
    for name in ["label", "label!", "until"] {
        for f in parsed.many("filter") {
            if let Some((n, v)) = f.split_once('=')
                && n == name
                && !filters.contains(&format!("{n}={v}"))
            {
                filters.push(format!("{n}={v}"));
            }
        }
    }
    filters.sort_by(|a, b| shards_cmdline::ports::natural_compare(a, b));
    filters
}

/// Whether, asked `question` on stdout, stdin answers yes (`y`, as docker/cli's
/// PromptForConfirmation takes it, in either case).
fn confirmed(question: &str) -> bool {
    let _ = std::io::stdout().write_all(question.as_bytes());
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    answer.trim().eq_ignore_ascii_case("y")
}

/// Draws a page that changes as it is watched over the last one drawn into `live`: a
/// search's (D172), its results filled in as their lookups answer.
#[cfg(unix)]
pub(crate) fn show_live(sheet: &shards_ipc::Sheet, live: &mut shards_tui::frame::Frame) {
    screens::show(sheet, live);
}

/// What an option does, as a form's page says it: shards' own words for it where Docker's
/// say what shards does otherwise, Docker's made shards' elsewhere.
pub(crate) fn said_ours(docker: &str) -> String {
    shards_cmdline::grammar::option_ours(docker)
        .map_or_else(|| shards_cmdline::grammar::ours(docker), str::to_string)
}

/// The form of shards' grammar this command line said, where it said one (`list vms`):
/// its `--help` is that form's page.
static FORM: std::sync::OnceLock<shards_cmdline::grammar::Form> = std::sync::OnceLock::new();

/// Reads `argv` for `command`, which `path` names; or answers its `--help` or its
/// mistakes as the Docker CLI does, with the status to exit with.
pub(crate) fn read(
    command: &'static Command,
    path: &str,
    argv: &[String],
    validate: &dyn Fn(&flags::Flag, &str) -> Result<String, String>,
) -> Result<Parsed, ExitCode> {
    match flags::parse(command, path, argv, validate) {
        Outcome::Run(parsed) => Ok(parsed),
        Outcome::Help { notices } => {
            let _ = std::io::stdout().write_all(notices.as_bytes());
            let form = FORM.get();
            #[cfg(unix)]
            if let Some(p) = look::styled() {
                match form {
                    Some(form) => look::form(&p, command, form, &mut std::io::stdout().lock()),
                    None => look::command(&p, command, path, &mut std::io::stdout().lock()),
                }
                return Err(ExitCode::SUCCESS);
            }
            let help = match form {
                Some(form) => flags::form_help(command, form, columns(), &said_ours),
                None => flags::help(command, path, columns()),
            };
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
pub(crate) fn utc_offset() -> i32 {
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

/// The VM process's binary for `args`, which `shards` carries and writes out (helpers.rs):
/// the GPU's where its own options (those before `--`) ask for a GPU (D144).
fn vm_binary(args: &[OsString]) -> Result<PathBuf, String> {
    if args.iter().take_while(|a| *a != "--").any(|a| a == "--gpu") {
        crate::helpers::vm_gpu()
    } else {
        crate::helpers::vm()
    }
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
    let bin = match vm_binary(args) {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    let e = std::process::Command::new(&bin)
        .args(args)
        .env("MVK_CONFIG_LOG_LEVEL", MOLTENVK_LOG_LEVEL)
        .exec();
    failed(&format!("{}: {e}", bin.display()))
}

/// What a GPU's VM process says of MoltenVK: its errors alone (MVK_CONFIG_LOG_LEVEL 1, its
/// mvk_config.h), where its default, information, writes a page about the GPU as the
/// renderer starts.
#[cfg(unix)]
const MOLTENVK_LOG_LEVEL: &str = "1";

/// Runs the VM process's binary with `args`, and exits as it does: Windows has no exec.
#[cfg(not(unix))]
fn instead(args: &[OsString]) -> ExitCode {
    let bin = match vm_binary(args) {
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
    // A restore's template, whose files its broker grants beneath it alone (PM M165).
    let template = match (args.first().and_then(|a| a.to_str()), args.get(1)) {
        (Some("restore"), Some(dir)) => match std::path::absolute(dir) {
            Ok(dir) => Some(dir),
            Err(e) => return failed(&format!("{}: {e}", std::path::Path::new(dir).display())),
        },
        _ => None,
    };
    let mut broker_args: Vec<&std::ffi::OsStr> = vec!["grants".as_ref()];
    if let Some(dir) = &template {
        broker_args.extend(["--template".as_ref(), dir.as_os_str()]);
    }
    let spawned = shards_ipc::spawn(
        &broker,
        &broker_args,
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

/// `shards run --kernel` and `shards restore`: become shards-vm. A restore whose snapshot's
/// generation has no working set first starts its writer, `shardsd working-set`, on a pipe
/// the VM hands the working set it records to (`--working-set`): no VM writes its snapshot
/// (D30), and a process that becomes the VM leaves none behind to. The writer is reaped by
/// the kernel, since SIGCHLD stays ignored through the exec.
#[cfg(not(any(target_os = "macos", windows)))]
fn vm(args: &[OsString]) -> ExitCode {
    match working_set_writer(args) {
        Ok(Some(with)) => instead(&with),
        Ok(None) => instead(args),
        Err(e) => failed(&e),
    }
}

/// `args` with `--working-set FD` given, its writer started on the pipe's other end, where
/// they restore a snapshot whose generation has no working set and this backend records a
/// restore's (`vm::RESTORES_RECORD`); none otherwise, or where the writer cannot start: a
/// restore without one only takes the faults it would have.
#[cfg(target_os = "linux")]
fn working_set_writer(args: &[OsString]) -> Result<Option<Vec<OsString>>, String> {
    use std::os::fd::{AsFd as _, IntoRawFd as _};
    let spawned_by_daemon = args
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--warm" || a == "--resume" || a == "--working-set");
    let (Some(restore), Some(dir), false) = (args.first(), args.get(1), spawned_by_daemon) else {
        return Ok(None);
    };
    let dir = std::path::Path::new(dir);
    if restore != "restore"
        || !shards_vmm::vm::RESTORES_RECORD
        || !shards_vmm::snapshot::exists(dir)
        || shards_vmm::snapshot::has_working_set(dir)
    {
        return Ok(None);
    }
    let dir = std::path::absolute(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (from_vm, to_writer) = std::io::pipe().map_err(|e| format!("the working set's pipe: {e}"))?;
    let writer = shardsd()?;
    // SAFETY: signal(2) setting SIGCHLD's disposition, before any thread starts.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    let stderr = std::io::stderr();
    let spawned = shards_ipc::spawn(
        &writer,
        &["working-set".as_ref(), "--template".as_ref(), dir.as_os_str()],
        &[(from_vm.as_fd(), 3), (stderr.as_fd(), 2)],
        true,
    );
    if let Err(e) = spawned {
        shards_vmm::debug!(
            "not keeping this restore's working set: {}: {e}",
            writer.display()
        );
        return Ok(None);
    }
    drop(from_vm);
    // Kept open through the exec, for the VM.
    let fd = to_writer.into_raw_fd();
    // SAFETY: fcntl(2) clearing close-on-exec on a descriptor this process owns.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return Err(format!(
            "the working set's pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut with: Vec<OsString> = Vec::with_capacity(args.len() + 2);
    with.extend(args.first().cloned());
    with.push("--working-set".into());
    with.push(fd.to_string().into());
    with.extend(args.iter().skip(1).cloned());
    Ok(Some(with))
}

/// No other Unix but macOS records a restore's working set, so none starts a writer.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn working_set_writer(_: &[OsString]) -> Result<Option<Vec<OsString>>, String> {
    Ok(None)
}

/// `shards run --kernel` and `shards restore` on Windows: shards-vm, started confined (D141):
/// in an AppContainer of its own, which holds the hypervisor's capabilities and reaches
/// none of its user's files or sockets; in a Job that ends it as this process ends and lets
/// it start nothing; with Windows' mitigations (PM M200-M203). This process then answers
/// what it asks for, as macOS's broker does (`grant_answer`), until it has all it needs,
/// and exits as it does. Windows has no exec: the VM is a child, which takes the console's
/// Ctrl-C itself, while this process waits to say how it ended.
#[cfg(windows)]
fn vm(args: &[OsString]) -> ExitCode {
    use std::os::windows::io::{AsHandle as _, OwnedHandle};
    if !matches!(args.first().and_then(|a| a.to_str()), Some("run" | "restore")) {
        return instead(args);
    }
    let (args, served) = match served_pipes(args) {
        Ok(served) => served,
        Err(e) => return failed(&e),
    };
    let args = args.as_slice();
    let bin = match vm_binary(args) {
        Ok(bin) => bin,
        Err(e) => return failed(&e),
    };
    let container = match crate::helpers::containers().and_then(|records| {
        shards_ipc::AppContainer::new("vm", &records).map_err(|e| format!("the VM's AppContainer: {e}"))
    }) {
        Ok(container) => container,
        Err(e) => return failed(&e),
    };
    // A pipe each way: the VM's requests, and the answers.
    let pipes = std::io::pipe().and_then(|asks| Ok((asks, std::io::pipe()?)));
    let ((asks_from, asks_to), (answers_from, answers_to)) = match pipes {
        Ok(pipes) => pipes,
        Err(e) => return failed(&format!("the VM's grants: {e}")),
    };
    // The VM process starts where its container may not read: relative paths are of this
    // directory.
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => return failed(&format!("the working directory: {e}")),
    };
    let mut with: Vec<&std::ffi::OsStr> = Vec::with_capacity(args.len() + 2);
    with.extend(args.first().map(OsString::as_os_str));
    with.push("--cwd".as_ref());
    with.push(cwd.as_os_str());
    with.extend(args.iter().skip(1).map(OsString::as_os_str));
    let (asks_to, answers_from) = (OwnedHandle::from(asks_to), OwnedHandle::from(answers_from));
    let env = crate::netproc::child_env();
    let mut given = vec![
        shards_ipc::Given {
            name: "ask",
            handle: asks_to.as_handle(),
        },
        shards_ipc::Given {
            name: "answer",
            handle: answers_from.as_handle(),
        },
    ];
    given.extend(served.iter().map(|(name, handle)| shards_ipc::Given {
        name,
        handle: *handle,
    }));
    let spawned = shards_ipc::spawn(&shards_ipc::Spawn {
        program: &bin,
        args: &with,
        grants_at: 1,
        given: &given,
        stdin: Some(std::io::stdin().as_handle()),
        stdout: std::io::stdout().as_handle(),
        stderr: std::io::stderr().as_handle(),
        sockets: &[],
        env: &crate::netproc::env_pairs(&env),
        container: Some(&container),
        holds: shards_ipc::Holds::Hypervisor,
    });
    // The VM's ends are its own now: its exit ends the requests.
    drop((asks_to, answers_from));
    let child = match spawned {
        Ok(child) => child,
        Err(e) => return failed(&format!("{}: {e}", bin.display())),
    };
    // Its Ctrl-C is the VM's, which shares this console; made after it, so that it keeps
    // the default (SetConsoleCtrlHandler: the setting is inherited).
    // SAFETY: no handler, ignoring Ctrl-C in this process.
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 1) };
    let link = crate::grant::Link {
        from: std::fs::File::from(OwnedHandle::from(asks_from)),
        to: std::fs::File::from(OwnedHandle::from(answers_to)),
    };
    let ended = std::thread::scope(|scope| {
        let serving = std::thread::Builder::new()
            .name("grants".into())
            .spawn_scoped(scope, || crate::grant_answer::serve(&link, &child));
        let ended = child.wait();
        // What it could not grant, the VM was told and said.
        if let Ok(serving) = serving {
            let _ = serving.join();
        }
        ended
    });
    drop(container);
    match ended {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => failed(&format!("waiting for the VM: {e}")),
    }
}

/// A pipe a share another server serves rides (Windows): its name in the VM's grants, and
/// the handle this process inherited.
#[cfg(windows)]
type ServedPipe = (String, std::os::windows::io::BorrowedHandle<'static>);

/// The pipes `shards run … --slates-fs-handle V` names (D38, PM M340): each a handle this
/// process inherited from the server that serves its share (slates' `slates shards`). The
/// VM is given each through its grants as `slates-fs-<i>`, in its order, and told the name
/// with `--slates-fs`, as `--net`'s handles are given: no handle's value crosses as a flag,
/// since none is known in the child before the spawn duplicates it there. Each must be a
/// pipe this process holds, given once. The arguments the VM is given, and the pipes.
#[cfg(windows)]
fn served_pipes(args: &[OsString]) -> Result<(Vec<OsString>, Vec<ServedPipe>), String> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    let mut kept = Vec::with_capacity(args.len());
    let mut pipes: Vec<ServedPipe> = Vec::new();
    let mut values: Vec<usize> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg != "--slates-fs-handle" {
            kept.push(arg.clone());
            continue;
        }
        let given = rest
            .next()
            .and_then(|v| v.to_str())
            .ok_or("--slates-fs-handle: no handle given")?;
        let value: usize = given
            .parse()
            .map_err(|_| format!("--slates-fs-handle {given:?}: not a handle"))?;
        if values.contains(&value) {
            return Err(format!("--slates-fs-handle {value}: given twice"));
        }
        let raw = value as std::os::windows::io::RawHandle;
        // SAFETY: GetFileType reads a handle's type and touches no memory; a value this
        // process holds no handle at answers FILE_TYPE_UNKNOWN.
        if value == 0 || unsafe { GetFileType(raw) } != FILE_TYPE_PIPE {
            return Err(format!(
                "--slates-fs-handle {value}: not a pipe this process holds"
            ));
        }
        // SAFETY: a handle this process inherited for this share and never closes: it stays
        // open while the spawn duplicates it into the VM, and until this process ends.
        let handle = unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(raw) };
        let name = format!("slates-fs-{}", pipes.len());
        kept.push("--slates-fs".into());
        kept.push(name.clone().into());
        values.push(value);
        pipes.push((name, handle));
    }
    Ok((kept, pipes))
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}");
    ExitCode::FAILURE
}

#[cfg(all(test, windows))]
mod served_pipes_tests {
    //! `shards run … --slates-fs-handle V` on Windows (D38, PM M340): each pipe the spawner
    //! holds goes to the VM through its grants by name, never by value.
    use std::ffi::OsString;
    use std::os::windows::io::AsRawHandle as _;

    use super::served_pipes;

    fn args(of: &[&str]) -> Vec<OsString> {
        of.iter().map(OsString::from).collect()
    }

    /// A pipe this process holds goes to the VM as `slates-fs-0`, named with
    /// `--slates-fs`, the rest of the arguments kept in order.
    #[test]
    fn a_pipe_it_holds_goes_by_name() {
        let (read, _write) = std::io::pipe().unwrap();
        let value = (read.as_raw_handle() as usize).to_string();
        let (kept, pipes) = served_pipes(&args(&[
            "run",
            "--kernel",
            "/k",
            "--slates-fs-handle",
            &value,
            "--cpus",
            "2",
        ]))
        .unwrap();
        assert_eq!(
            kept,
            args(&[
                "run",
                "--kernel",
                "/k",
                "--slates-fs",
                "slates-fs-0",
                "--cpus",
                "2"
            ])
        );
        assert_eq!(pipes.len(), 1);
        assert_eq!(pipes[0].0, "slates-fs-0");
    }

    /// No value, one that is no number, a handle that is no pipe, and a pipe given twice
    /// are each refused, the flag named.
    #[test]
    fn what_is_no_pipe_it_holds_is_refused() {
        let (read, _write) = std::io::pipe().unwrap();
        let pipe = (read.as_raw_handle() as usize).to_string();
        let file = std::fs::File::open("NUL").unwrap();
        let not_pipe = (file.as_raw_handle() as usize).to_string();
        for (given, why) in [
            (vec!["run", "--slates-fs-handle"], "no handle given"),
            (vec!["run", "--slates-fs-handle", "x"], "not a handle"),
            (
                vec!["run", "--slates-fs-handle", not_pipe.as_str()],
                "not a pipe this process holds",
            ),
            (
                vec!["run", "--slates-fs-handle", "0"],
                "not a pipe this process holds",
            ),
            (
                vec![
                    "run",
                    "--slates-fs-handle",
                    pipe.as_str(),
                    "--slates-fs-handle",
                    pipe.as_str(),
                ],
                "given twice",
            ),
        ] {
            let e = served_pipes(&args(&given)).err().unwrap();
            assert!(
                e.contains("--slates-fs-handle") && e.contains(why),
                "{given:?}: {e}"
            );
        }
    }
}
