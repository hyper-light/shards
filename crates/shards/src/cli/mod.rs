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
mod cp;
#[cfg(unix)]
pub(crate) mod listing;
#[cfg(unix)]
pub(crate) mod look;
pub(crate) mod request;
#[cfg(unix)]
mod save;
#[cfg(unix)]
mod screens;
#[cfg(unix)]
mod show;
#[cfg(unix)]
mod terminal;
pub(crate) mod version;

/// `docker run`'s status when it could not run the command at all.
const NOT_RUN: u8 = 125;

pub fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    dispatch(args)
}

/// The commands shards has besides the catalog's: the daemon side's own words.
/// `daemon` and `guest` are what `run daemon`, `stop daemon` and `configure guest` are
/// said as, and how the client starts the daemon.
const OWN: [&str; 15] = [
    "system",
    "agent",
    "harness",
    "mcp",
    "daemon",
    "guest",
    "grants",
    "share",
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
    // An action of shards' own alone, or asked for its help: its page.
    if let [action] | [action, _] = text.as_slice()
        && matches!(action.as_str(), "list" | "remove" | "configure")
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
        // The page is named as it was asked for: `list vms`, not `ps`.
        #[cfg(unix)]
        look::name_as(&text.iter().take(2).cloned().collect::<Vec<_>>().join(" "));
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
        [name @ ("image" | "container" | "volume")]
        | [name @ ("image" | "container" | "volume"), "-h" | "--help"] => {
            return management_help(name);
        }
        [name @ ("image" | "container" | "volume"), word, ..]
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
        // `shards restore DIR`: a microVM resumed from its snapshot.
        ["restore", ..] => vm(&args),
        // This shards' own version: nothing asked of a daemon (version.rs).
        ["version", ..] => version::run(args.get(1..).unwrap_or_default()),
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
    let parsed = match read(command, path, &argv, &flags::value) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let _ = std::io::stdout().write_all(parsed.notices.as_bytes());
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
    // `volume prune --all` and a filter `all` both: the CLI's own refusal (volume/prune.go).
    if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_PRUNE)
        && parsed.bool("all")
        && parsed
            .many("filter")
            .iter()
            .any(|f| f.split_once('=').is_some_and(|(k, _)| k == "all"))
    {
        let _ = writeln!(
            std::io::stderr(),
            "conflicting options: cannot specify both --all and --filter all=1"
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
            // The flag's default is DOCKER_DEFAULT_PLATFORM.
            if !parsed.changed("platform")
                && let Ok(p) = std::env::var("DOCKER_DEFAULT_PLATFORM")
                && !p.is_empty()
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
                let mut fds: Vec<std::os::fd::BorrowedFd<'_>> = output.iter().map(save::Output::fd).collect();
                match &input {
                    Some(Some(file)) => fds.push(file.as_fd()),
                    Some(None) => fds.push(stdin.as_fd()),
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
