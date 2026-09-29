//! `shards run IMAGE [COMMAND] [ARG...]`: runs a command in a new microVM booted into an
//! image, as `docker run` runs one in a new container (docs/design/architecture.md D16,
//! D24). The image's entrypoint, command, environment, working directory and user apply
//! unless the command line gives its own, merged as dockerd merges them.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use shards_image::oci::RunConfig;
use shards_image::reference::Reference;
use shards_registry::pull::{Event, local};

use crate::workload::{NOT_RUN, Options};

const USAGE: &str = "usage: shards run [OPTIONS] IMAGE [COMMAND] [ARG...]
  Runs COMMAND in a new microVM booted into IMAGE, as `docker run` runs it in a new
  container. The image's entrypoint, command, environment, working directory and user
  apply unless given here. IMAGE is pulled first if it is not here.
  Options, as for `docker run`: -e NAME[=VALUE], -w DIR, -u USER[:GROUP], --hostname NAME,
  -i, --entrypoint COMMAND, --pull missing|always|never, --rm (nothing outlives a run).
  --kernel FILE, --init FILE: the guest's kernel and shards-init; else SHARDS_KERNEL and
  SHARDS_INIT.";

/// What the command line asks of a run.
#[derive(Debug, Default)]
struct Asked {
    env: Vec<String>,
    workdir: String,
    user: String,
    hostname: Option<String>,
    interactive: bool,
    /// `--entrypoint`: `None` when not given; empty when given as "".
    entrypoint: Option<Vec<String>>,
    cmd: Vec<String>,
    pull: Pull,
    kernel: Option<PathBuf>,
    init: Option<PathBuf>,
    image: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Pull {
    #[default]
    Missing,
    Always,
    Never,
}

pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let asked = match parse(args) {
        Ok(asked) => asked,
        Err(e) if e.is_empty() => {
            let _ = writeln!(std::io::stdout(), "{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}\n{USAGE}");
            return ExitCode::from(NOT_RUN);
        }
    };
    match prepare(&asked) {
        Ok((cfg, rootfs, workload)) => crate::vm_run::run_in(cfg, rootfs, &workload),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}");
            ExitCode::from(NOT_RUN)
        }
    }
}

/// Options come before the image, as `docker run` takes them; what follows the image is
/// the command.
fn parse(args: impl Iterator<Item = OsString>) -> Result<Asked, String> {
    let mut args = args.map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
    });
    let mut asked = Asked::default();
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
            "--kernel" => asked.kernel = Some(PathBuf::from(value("--kernel")?)),
            "--init" => asked.init = Some(PathBuf::from(value("--init")?)),
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

/// The VM, its root filesystem, and the workload a run asks for.
fn prepare(asked: &Asked) -> Result<(shards_vmm::vm::Config, PathBuf, Options), String> {
    let from_env = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let kernel = asked
        .kernel
        .clone()
        .or_else(|| from_env("SHARDS_KERNEL"))
        .ok_or("a guest kernel is required: --kernel or SHARDS_KERNEL")?;
    let init = asked
        .init
        .clone()
        .or_else(|| from_env("SHARDS_INIT"))
        .ok_or("shards-init is required: --init or SHARDS_INIT")?;
    let reference = Reference::parse(&asked.image).map_err(|e| e.to_string())?;
    let store = crate::pull::store()?;
    let found = match asked.pull {
        Pull::Always => None,
        Pull::Missing | Pull::Never => local(&store, &reference, u64::MAX).map_err(|e| e.to_string())?,
    };
    let image = match found {
        Some(image) => image,
        None if asked.pull == Pull::Never => {
            return Err(format!("No such image: {}", reference.familiar()));
        }
        None => {
            // `docker run` pulls as `docker pull` does, on stderr.
            let say = |line: &str| {
                let _ = writeln!(std::io::stderr(), "{line}");
            };
            if asked.pull == Pull::Missing {
                say(&format!(
                    "Unable to find image '{}' locally",
                    reference.familiar()
                ));
            }
            let report = |event: Event<'_>| match event {
                Event::Layer(d) => say(&format!("{}: Download complete", short(&d.to_string()))),
                Event::Present(d) => say(&format!("{}: Already exists", short(&d.to_string()))),
                Event::Manifest(..) | Event::Progress(..) | Event::Building => {}
            };
            let (pulled, _) = crate::pull::fetch(&reference, &report, &say)?;
            say(&format!("Digest: {}", pulled.resolved));
            say(&format!(
                "Status: Downloaded newer image for {}",
                reference.familiar()
            ));
            pulled
        }
    };
    let workload = compose(image.config.config.as_ref(), asked)?;
    Ok((crate::vm_run::config(kernel, Some(init)), image.rootfs, workload))
}

/// The workload: the command line's settings merged over the image's, as dockerd merges
/// them (moby docker-v29.8.1 `daemon/commit.go`, `merge`).
/// - The user and working directory are the image's unless given.
/// - The environment is the given variables, then each of the image's whose name was not
///   given. dockerd then lays it over its PATH and HOSTNAME (workload.rs).
/// - The image's command applies only when neither an entrypoint nor a command is given.
///   Its entrypoint applies unless one is given, even an empty one.
fn compose(image: Option<&RunConfig>, asked: &Asked) -> Result<Options, String> {
    let image = image.cloned().unwrap_or_default();
    let mut env = asked.env.clone();
    for entry in image.env.unwrap_or_default() {
        let name = entry.split('=').next().unwrap_or_default();
        if !env.iter().any(|given| given.split('=').next() == Some(name)) {
            env.push(entry);
        }
    }
    let (entrypoint, cmd) = match &asked.entrypoint {
        Some(given) if !given.is_empty() => (given.clone(), asked.cmd.clone()),
        given => {
            let cmd = if asked.cmd.is_empty() {
                image.cmd.unwrap_or_default()
            } else {
                asked.cmd.clone()
            };
            let entrypoint = match given {
                None => image.entrypoint.unwrap_or_default(),
                Some(empty) => empty.clone(),
            };
            (entrypoint, cmd)
        }
    };
    let argv: Vec<String> = entrypoint.into_iter().chain(cmd).collect();
    if argv.is_empty() {
        return Err("no command specified".into());
    }
    Ok(Options {
        argv,
        env,
        workdir: if asked.workdir.is_empty() {
            image.working_dir.unwrap_or_default()
        } else {
            asked.workdir.clone()
        },
        user: if asked.user.is_empty() {
            image.user.unwrap_or_default()
        } else {
            asked.user.clone()
        },
        hostname: asked.hostname.clone(),
        interactive: asked.interactive,
    })
}

/// Docker's short layer ID: the digest's first 12 hex digits.
fn short(digest: &str) -> &str {
    let hex = digest.split_once(':').map_or(digest, |(_, h)| h);
    hex.get(..12).unwrap_or(hex)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn image() -> RunConfig {
        RunConfig {
            user: Some("app".into()),
            env: Some(strings(&["PATH=/usr/bin:/bin", "LANG=C.UTF-8", "MODE=image"])),
            entrypoint: Some(strings(&["/entry.sh"])),
            cmd: Some(strings(&["serve", "--port", "80"])),
            working_dir: Some("/srv".into()),
        }
    }

    /// dockerd's merge, case by case (moby `daemon/commit.go`).
    #[test]
    fn images_and_command_lines_merge_as_dockerd_merges_them() {
        let run = |asked: Asked| compose(Some(&image()), &asked).unwrap();
        let plain = run(Asked::default());
        assert_eq!(plain.argv, strings(&["/entry.sh", "serve", "--port", "80"]));
        assert_eq!((plain.user.as_str(), plain.workdir.as_str()), ("app", "/srv"));
        assert_eq!(
            plain.env,
            strings(&["PATH=/usr/bin:/bin", "LANG=C.UTF-8", "MODE=image"])
        );
        // A command replaces the image's command, and keeps its entrypoint.
        let command = run(Asked {
            cmd: strings(&["migrate"]),
            ..Asked::default()
        });
        assert_eq!(command.argv, strings(&["/entry.sh", "migrate"]));
        // An entrypoint drops the image's command too.
        let entry = run(Asked {
            entrypoint: Some(strings(&["/bin/sh"])),
            ..Asked::default()
        });
        assert_eq!(entry.argv, strings(&["/bin/sh"]));
        // An empty entrypoint clears the image's, and keeps its command.
        let cleared = run(Asked {
            entrypoint: Some(Vec::new()),
            ..Asked::default()
        });
        assert_eq!(cleared.argv, strings(&["serve", "--port", "80"]));
        // Given variables come first; the image's fill in the names not given.
        let env = run(Asked {
            env: strings(&["MODE=given", "EXTRA=1", "LANG"]),
            user: "root".into(),
            workdir: "/tmp".into(),
            ..Asked::default()
        });
        assert_eq!(
            env.env,
            strings(&["MODE=given", "EXTRA=1", "LANG", "PATH=/usr/bin:/bin"])
        );
        assert_eq!((env.user.as_str(), env.workdir.as_str()), ("root", "/tmp"));
        // Nothing to run at all.
        assert!(compose(None, &Asked::default()).is_err());
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
