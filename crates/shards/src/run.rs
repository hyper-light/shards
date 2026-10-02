//! `shards run`, the daemon's half (daemon.rs): a request (`shards_ipc::Run`) made ready
//! to run, as `docker run` runs one in a new container (docs/design/architecture.md D16,
//! D24–D26). The image's entrypoint, command, environment, working directory and user
//! apply unless the command line gave its own, merged as dockerd merges them. With the
//! recorded guest (guest.rs), a run's VM comes from a template of the image, booted and
//! mounted. A template is kept by content: the guest's digests, the image's root
//! filesystem, and the VM's shape. The command line's half is the `shards` binary's
//! (src/bin/shards/request.rs).

// Where shards has no daemon yet (Windows), nothing prepares a run.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use shards_abi::run::Spec;
use shards_image::oci::RunConfig;
use shards_image::platform;
use shards_image::reference::Reference;
use shards_image::store::Lease;
use shards_ipc::{Pull, Run};
use shards_registry::ErrorKind;
use shards_registry::http::Cancel;
use shards_registry::pull::{Event, local};
use shards_vmm::vm::Config;

use crate::guest::Guest;
use crate::spec::Options;

/// What a run boots: the kernel and init the request named, or a guest from the store:
/// the recorded one, else the default (D28).
pub enum Boot {
    Given(Config),
    Stored(Guest),
}

/// A request made ready to run.
pub struct Prepared {
    pub boot: Boot,
    /// The image's root filesystem.
    pub rootfs: PathBuf,
    pub spec: Spec,
    /// The command reads the client's stdin.
    pub interactive: bool,
    /// The image store's lease, held until the run's VM has its root filesystem: no
    /// collection removes it meanwhile, whatever its reference names by then.
    pub lease: Option<Lease>,
    /// The signal `stop` sends unless told: the run's, else its image's.
    pub stop_signal: Option<String>,
    /// What the run was composed of: the base of each `exec` in its container.
    pub options: crate::spec::Options,
    /// Its health check: the run's merged with its image's.
    pub health: Option<shards_ipc::Health>,
    /// The shell a `CMD-SHELL` health check runs in: the image's `SHELL`, or `/bin/sh -c`.
    pub shell: Vec<String>,
    /// The image's `EXPOSE`d ports, `80/tcp` and the like.
    pub exposed: Vec<String>,
    /// The image's ID: what its reference resolved to.
    pub image_id: String,
}

/// The health check a run's container has, as dockerd merges the run's with its image's
/// (moby daemon/commit.go, merge): the run's, or the image's where the run gives none; a
/// run's test that is empty, and each of its durations and retries that is 0, the
/// image's.
fn merge_health(
    run: Option<&shards_ipc::Health>,
    image: Option<&shards_image::oci::HealthConfig>,
) -> Option<shards_ipc::Health> {
    let image = image.map(|h| shards_ipc::Health {
        test: h.test.clone().unwrap_or_default(),
        interval: h.interval,
        timeout: h.timeout,
        start_period: h.start_period,
        start_interval: h.start_interval,
        retries: h.retries,
    });
    let Some(run) = run else {
        return image;
    };
    let Some(image) = image else {
        return Some(run.clone());
    };
    let or = |given: i64, theirs: i64| if given == 0 { theirs } else { given };
    Some(shards_ipc::Health {
        test: if run.test.is_empty() {
            image.test
        } else {
            run.test.clone()
        },
        interval: or(run.interval, image.interval),
        timeout: or(run.timeout, image.timeout),
        start_period: or(run.start_period, image.start_period),
        start_interval: or(run.start_interval, image.start_interval),
        retries: or(run.retries, image.retries),
    })
}

/// The daemon's half: finds the request's image in `home`, pulling it as `docker run`
/// does with its messages through `say`, and merges its settings under the request's.
/// Whatever it downloads, `cancel` stops.
pub fn prepare(
    request: &Run,
    home: &Path,
    say: &(dyn Fn(&str) + Sync),
    cancel: &Cancel,
) -> Result<Prepared, String> {
    let boot = match (&request.kernel, &request.init) {
        (Some(kernel), Some(init)) => {
            Boot::Given(Config::new(PathBuf::from(kernel), Some(PathBuf::from(init))))
        }
        (None, None) => Boot::Stored(match crate::guest::current(home)? {
            Some(recorded) => recorded,
            None => crate::guest::default(home, say, Some(cancel))?,
        }),
        _ => return Err("--kernel and --init (or SHARDS_KERNEL and SHARDS_INIT) go together".into()),
    };
    let asked = request;
    let reference = Reference::parse(&asked.image).map_err(|e| e.to_string())?;
    let store = crate::pull::store(home)?;
    let lease = store.lease().map_err(|e| e.to_string())?;
    let mut changed = false;
    let found = match asked.pull {
        Pull::Always => None,
        Pull::Missing | Pull::Never => {
            let limits = crate::pull::limits()?;
            match local(&store, &reference, &platform::guest(), &limits) {
                // A stored copy that has changed is fetched again, as a pull mends it.
                Err(e) if e.kind() == ErrorKind::Changed && asked.pull == Pull::Missing => {
                    say(&format!("{e}; pulling '{}' again", reference.familiar()));
                    changed = true;
                    None
                }
                Err(e) if e.kind() == ErrorKind::Changed => {
                    return Err(format!("{e}; pull '{}' again to mend it", reference.familiar()));
                }
                found => found.map_err(|e| e.to_string())?,
            }
        }
    };
    let image = match found {
        Some(image) => image,
        None if asked.pull == Pull::Never => {
            return Err(format!("No such image: {}", reference.familiar()));
        }
        None => {
            // `docker run` pulls as `docker pull` does, on stderr.
            if asked.pull == Pull::Missing && !changed {
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
            let (pulled, _) = crate::pull::fetch(home, &reference, &report, &say, Some(cancel))?;
            say(&format!("Digest: {}", pulled.resolved));
            say(&format!(
                "Status: Downloaded newer image for {}",
                reference.familiar()
            ));
            pulled
        }
    };
    let options = compose(image.config.config.as_ref(), request)?;
    let stop_signal = request
        .stop_signal
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| image.config.config.as_ref().and_then(|c| c.stop_signal.clone()))
        .filter(|s| !s.is_empty());
    // The client gave `-e NAME` its value already: this process's environment is not
    // the user's.
    let spec = crate::spec::spec(&options, |_| None)?;
    Ok(Prepared {
        boot,
        rootfs: image.rootfs,
        spec,
        interactive: options.interactive,
        lease: Some(lease),
        stop_signal,
        options,
        health: merge_health(
            request.health.as_ref(),
            image.config.config.as_ref().and_then(|c| c.healthcheck.as_ref()),
        ),
        shell: image
            .config
            .config
            .as_ref()
            .and_then(|c| c.shell.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| vec!["/bin/sh".into(), "-c".into()]),
        exposed: image.config.config.map(|c| c.exposed_ports).unwrap_or_default(),
        image_id: image.resolved.to_string(),
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

/// What a template was saved from, recorded in it: its root filesystem and its guest. It
/// is live while that root filesystem is there and that guest is the current one
/// (daemon.rs, `collect_garbage`).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Origin {
    rootfs: PathBuf,
    kernel_digest: String,
    init_digest: String,
}

impl Origin {
    const FILE: &str = "origin.json";

    pub fn of(guest: &Guest, rootfs: &Path) -> Origin {
        Origin {
            rootfs: rootfs.to_path_buf(),
            kernel_digest: guest.kernel_digest.clone(),
            init_digest: guest.init_digest.clone(),
        }
    }

    /// Records it in the template `dir`.
    pub fn write(&self, dir: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(Self::FILE), bytes).map_err(|e| e.to_string())
    }

    /// What the template `dir` records, if it records it whole.
    pub fn read(dir: &Path) -> Option<Origin> {
        let bytes = std::fs::read(dir.join(Self::FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn live(&self, guest: Option<&Guest>) -> bool {
        self.rootfs.is_file()
            && guest
                .is_some_and(|g| g.kernel_digest == self.kernel_digest && g.init_digest == self.init_digest)
    }
}

/// Makes a freshly saved template the template, unless another run's got there first; a
/// save that did not complete is removed.
pub fn settle(fresh: &Path, dir: &Path) {
    if shards_vmm::snapshot::exists(fresh) && !dir.exists() && std::fs::rename(fresh, dir).is_ok() {
        return;
    }
    let _ = std::fs::remove_dir_all(fresh);
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
        tty: asked.tty.map(|(rows, cols)| shards_abi::run::Size { rows, cols }),
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
            stop_signal: None,
            healthcheck: None,
            shell: None,
            exposed_ports: Vec::new(),
        }
    }

    /// A run's health check merged with its image's as dockerd merges them (measured
    /// against dockerd 29.3.1, 2026-10-02).
    #[test]
    fn health_checks_merge_as_dockerd_merges_them() {
        let image = shards_image::oci::HealthConfig {
            test: Some(vec!["CMD-SHELL".into(), "echo img".into()]),
            interval: 7_000_000_000,
            timeout: 3_000_000_000,
            retries: 4,
            ..Default::default()
        };
        let run = |h: shards_ipc::Health| merge_health(Some(&h), Some(&image)).unwrap();
        let cmd = |c: &str| vec!["CMD-SHELL".to_string(), c.to_string()];
        let merged = run(shards_ipc::Health {
            test: cmd("echo run"),
            retries: 9,
            ..Default::default()
        });
        assert_eq!(
            (merged.test, merged.interval, merged.timeout, merged.retries),
            (cmd("echo run"), 7_000_000_000, 3_000_000_000, 9)
        );
        let merged = run(shards_ipc::Health {
            timeout: 2_000_000_000,
            ..Default::default()
        });
        assert_eq!(
            (merged.test, merged.timeout, merged.retries),
            (cmd("echo img"), 2_000_000_000, 4)
        );
        let none = run(shards_ipc::Health {
            test: vec!["NONE".into()],
            ..Default::default()
        });
        assert_eq!(
            (none.test, none.interval),
            (vec!["NONE".to_string()], 7_000_000_000)
        );
        assert_eq!(merge_health(None, None), None);
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
}
