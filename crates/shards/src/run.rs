//! `shards run IMAGE [COMMAND] [ARG...]`: runs a command in a new microVM booted into an
//! image, as `docker run` runs one in a new container (docs/design/architecture.md D16,
//! D24–D26). The image's entrypoint, command, environment, working directory and user
//! apply unless the command line gives its own, merged as dockerd merges them.
//!
//! The command line becomes a request (`shards_ipc::Run`) that the daemon serves from a
//! warm VM (client.rs, daemon.rs). This module holds both halves: the request as this
//! process makes it, and the daemon's preparation of it. With the recorded guest
//! (guest.rs), a run's VM comes from a template of the image, booted and mounted. A
//! template is kept by content: the guest's digests, the image's root filesystem, and
//! the VM's shape.

// Where shards has no daemon yet (Windows), only the request is made.
#![cfg_attr(not(unix), allow(dead_code))]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest as _, Sha256};
use shards_abi::run::Spec;
use shards_image::oci::RunConfig;
use shards_image::reference::Reference;
#[cfg(unix)]
use shards_ipc::Identity;
use shards_ipc::{Pull, Run};
use shards_registry::pull::{Event, local};
use shards_vmm::vm::Config;

use crate::guest::Guest;
use crate::workload::{NOT_RUN, Options};

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
/// - the daemon binary it would start, this one.
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
    let daemon = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    // Only Unix has the daemon, so far.
    #[cfg(unix)]
    {
        request.daemon = Identity::of(&daemon).map_err(|e| format!("{}: {e}", daemon.display()))?;
    }
    Ok((crate::pull::home()?, daemon))
}

/// What a run boots: the kernel and init the request named, or the recorded guest.
pub enum Boot {
    Given(Config),
    Recorded(Guest),
}

/// A request made ready to run.
pub struct Prepared {
    pub boot: Boot,
    /// The image's root filesystem.
    pub rootfs: PathBuf,
    pub spec: Spec,
    /// The command reads the client's stdin.
    pub interactive: bool,
}

/// The daemon's half: finds the request's image in `home`, pulling it as `docker run`
/// does with its messages through `say`, and merges its settings under the request's.
pub fn prepare(request: &Run, home: &Path, say: &(dyn Fn(&str) + Sync)) -> Result<Prepared, String> {
    let boot = match (&request.kernel, &request.init) {
        (Some(kernel), Some(init)) => Boot::Given(crate::vm_run::config(
            PathBuf::from(kernel),
            Some(PathBuf::from(init)),
        )),
        (None, None) => Boot::Recorded(crate::guest::current(home)?.ok_or(
            "no guest to boot: `shards guest use --kernel FILE --init FILE`, or --kernel and --init",
        )?),
        _ => return Err("--kernel and --init (or SHARDS_KERNEL and SHARDS_INIT) go together".into()),
    };
    let asked = request;
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
    let options = compose(image.config.config.as_ref(), request)?;
    let spec = crate::workload::spec_given(&options)?;
    Ok(Prepared {
        boot,
        rootfs: image.rootfs,
        spec,
        interactive: options.interactive,
    })
}

/// The template a run of `rootfs` on `guest` with `cfg`'s shape restores: named by the
/// SHA-256 of everything that goes into it, the snapshot format included.
pub fn template(home: &Path, guest: &Guest, rootfs: &Path, cfg: &Config) -> PathBuf {
    let key = format!(
        "snapshot format {}\nkernel {}\ninit {}\nrootfs {}\ncpus {}\nmemory {}\ncmdline {}\n",
        shards_vmm::snapshot::FORMAT,
        guest.kernel_digest,
        guest.init_digest,
        rootfs.display(),
        cfg.vcpus,
        cfg.memory_mib,
        cfg.cmdline
    );
    let hex: String = Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    home.join("templates").join(hex)
}

/// Makes a freshly saved template the template, unless another run's got there first; a
/// save that did not complete is removed.
pub fn settle(fresh: &Path, dir: &Path) {
    if fresh.join(shards_vmm::snapshot::STATE).is_file()
        && !dir.exists()
        && std::fs::rename(fresh, dir).is_ok()
    {
        return;
    }
    let _ = std::fs::remove_dir_all(fresh);
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

/// The workload: the command line's settings merged over the image's, as dockerd merges
/// them (moby docker-v29.8.1 `daemon/commit.go`, `merge`).
/// - The user and working directory are the image's unless given.
/// - The environment is the given variables, then each of the image's whose name was not
///   given. dockerd then lays it over its PATH and HOSTNAME (workload.rs).
/// - The image's command applies only when neither an entrypoint nor a command is given.
///   Its entrypoint applies unless one is given, even an empty one.
fn compose(image: Option<&RunConfig>, asked: &Run) -> Result<Options, String> {
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
        let run = |asked: Run| compose(Some(&image()), &asked).unwrap();
        let plain = run(Run::default());
        assert_eq!(plain.argv, strings(&["/entry.sh", "serve", "--port", "80"]));
        assert_eq!((plain.user.as_str(), plain.workdir.as_str()), ("app", "/srv"));
        assert_eq!(
            plain.env,
            strings(&["PATH=/usr/bin:/bin", "LANG=C.UTF-8", "MODE=image"])
        );
        // A command replaces the image's command, and keeps its entrypoint.
        let command = run(Run {
            cmd: strings(&["migrate"]),
            ..Run::default()
        });
        assert_eq!(command.argv, strings(&["/entry.sh", "migrate"]));
        // An entrypoint drops the image's command too.
        let entry = run(Run {
            entrypoint: Some(strings(&["/bin/sh"])),
            ..Run::default()
        });
        assert_eq!(entry.argv, strings(&["/bin/sh"]));
        // An empty entrypoint clears the image's, and keeps its command.
        let cleared = run(Run {
            entrypoint: Some(Vec::new()),
            ..Run::default()
        });
        assert_eq!(cleared.argv, strings(&["serve", "--port", "80"]));
        // Given variables come first; the image's fill in the names not given.
        let env = run(Run {
            env: strings(&["MODE=given", "EXTRA=1", "LANG"]),
            user: "root".into(),
            workdir: "/tmp".into(),
            ..Run::default()
        });
        assert_eq!(
            env.env,
            strings(&["MODE=given", "EXTRA=1", "LANG", "PATH=/usr/bin:/bin"])
        );
        assert_eq!((env.user.as_str(), env.workdir.as_str()), ("root", "/tmp"));
        // Nothing to run at all.
        assert!(compose(None, &Run::default()).is_err());
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
