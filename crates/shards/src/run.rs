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
    Given(Box<Config>),
    Stored(Guest),
}

/// The kernel a run boots.
pub fn kernel_of(boot: &Boot) -> &std::path::Path {
    match boot {
        Boot::Given(cfg) => &cfg.kernel,
        Boot::Stored(guest) => &guest.kernel,
    }
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
    /// Its labels: the image's, and the run's over them (daemon/commit.go, merge).
    pub labels: std::collections::BTreeMap<String, String>,
    /// The image's ID: what its reference resolved to.
    pub image_id: String,
    /// The microVM's vCPUs and memory in MiB, as its resource limits need them.
    pub size: (u32, u64),
    /// The image's `VOLUME`s.
    pub image_volumes: Vec<String>,
    /// Its Agentfile, where it is an Agentfile's image (D109): what the daemon holds the
    /// run to, read in its root and checked against its digest, never from its labels.
    pub agentfile: Option<crate::agentfile::Agentfile>,
    /// How many directories it shares with its guest (D38): set once its mount points are.
    pub shares: u32,
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

/// Image `name` as a microVM in `home`, as `docker run` finds its image: here, or, as
/// `pull` says, pulled and made a microVM on the way (a container image is converted as
/// it comes), its messages through `say`; with the lease that holds it. Every action on
/// an image that is not here yet converts it this way.
pub fn find_image(
    name: &str,
    pull: Pull,
    targets: &[shards_image::platform::Target],
    registry_env: &[String],
    home: &Path,
    say: &(dyn Fn(&str) + Sync),
    cancel: &Cancel,
) -> Result<(shards_registry::pull::Pulled, Lease), String> {
    let store = crate::pull::store(home)?;
    let lease = store.lease().map_err(|e| e.to_string())?;
    // An image named by its ID, or a prefix of it, as dockerd's resolveImage takes one: a
    // name tagged here first, then the ID.
    if pull != Pull::Always
        && let Some(found) = by_id(&store, name, targets)?
    {
        return Ok((found, lease));
    }
    let reference = Reference::parse(name).map_err(|e| e.to_string())?;
    let mut changed = false;
    let found = match pull {
        Pull::Always => None,
        Pull::Missing | Pull::Never => {
            let limits = crate::pull::limits()?;
            match local(&store, &reference, targets, &limits) {
                // A stored copy that has changed is fetched again, as a pull mends it.
                Err(e) if e.kind() == ErrorKind::Changed && pull == Pull::Missing => {
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
        None if pull == Pull::Never => {
            return Err(format!("No such image: {}", reference.familiar()));
        }
        None => {
            // `docker run` pulls as `docker pull` does, on stderr.
            if pull == Pull::Missing && !changed {
                say(&format!(
                    "Unable to find image '{}' locally",
                    reference.familiar()
                ));
            }
            let report = |event: Event<'_>| match event {
                Event::Layer(d) => say(&format!("{}: Download complete", short(&d.to_string()))),
                Event::Present(d) => say(&format!("{}: Already exists", short(&d.to_string()))),
                Event::Manifest(..)
                | Event::Progress(..)
                | Event::Building
                | Event::Unpacking(_)
                | Event::Pulling => {}
            };
            let (pulled, _) = crate::pull::fetch(
                home,
                &reference,
                targets,
                &report,
                &say,
                Some(cancel),
                &|k| shards_ipc::env_value(registry_env, k),
                false,
            )?;
            say(&format!("Digest: {}", pulled.resolved));
            say(&format!(
                "Status: Downloaded newer image for {}",
                reference.familiar()
            ));
            pulled
        }
    };
    Ok((image, lease))
}

/// The image here `given` names by its ID (`sha256:` and 64 hex digits, or 4 to 64 of
/// them) where no image is tagged by that name, as dockerd's containerd store resolves one
/// (moby daemon/containerd/image.go, resolveImage). None where it names none: the name is
/// a reference, to find or pull.
#[cfg(unix)]
fn by_id(
    store: &shards_image::store::Store,
    given: &str,
    targets: &[shards_image::platform::Target],
) -> Result<Option<shards_registry::pull::Pulled>, String> {
    use shards_image::reference::AnyReference;
    let digest = matches!(AnyReference::parse(given), Ok(AnyReference::Digest(_)));
    if !digest && crate::daemon::images::truncated_id(given).is_none() {
        return Ok(None);
    }
    let named = store.named().map_err(|e| e.to_string())?;
    if !digest
        && let Ok(r) = Reference::parse(given)
        && named.iter().any(|i| i.references.contains(&r.to_string()))
    {
        return Ok(None);
    }
    let Ok(image) = crate::daemon::images::resolve(&named, given) else {
        return Ok(None);
    };
    let Some(tagged) = image.references.first() else {
        return Ok(None);
    };
    let limits = crate::pull::limits()?;
    shards_registry::pull::local_tagged(store, tagged, given, targets, &limits).map_err(|e| e.to_string())
}

/// Where no daemon serves, none: an image is found by its reference.
#[cfg(not(unix))]
fn by_id(
    _: &shards_image::store::Store,
    _: &str,
    _: &[shards_image::platform::Target],
) -> Result<Option<shards_registry::pull::Pulled>, String> {
    Ok(None)
}

/// The image's labels, with the run's `--label`/`--label-file` over them (opts.
/// ConvertKVStringsToMap: `KEY` alone is an empty value). The `vnd.osi.agentfile.*`
/// namespace is the build's record of the Agentfile, which the daemon and the guest
/// trust: a run may not set, remove or forge one, so it is refused here (the user mutates
/// a run through the CLI), as the build refuses it in a `LABEL` or a `--label`.
fn merge_labels(
    mut labels: std::collections::BTreeMap<String, String>,
    request: &[String],
) -> Result<std::collections::BTreeMap<String, String>, String> {
    for l in request {
        let (k, v) = l.split_once('=').unwrap_or((l.as_str(), ""));
        if shards_dockerfile::agentfile::reserved_label(k.as_bytes()) {
            return Err(format!(
                "label {k:?} is reserved: shards' build sets the {} labels from the Agentfile, and a run may not set one",
                String::from_utf8_lossy(shards_dockerfile::agentfile::LABEL_PREFIX)
            ));
        }
        labels.insert(k.to_string(), v.to_string());
    }
    Ok(labels)
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
    // `-q`: what a pull says goes unsaid (docker/cli create.go pullImage); errors do not.
    let quiet = |line: &str| {
        if !request.quiet {
            say(line);
        }
    };
    let say: &(dyn Fn(&str) + Sync) = &quiet;
    // `--platform`'s image, else the best this host's microVMs run.
    let targets = crate::pull::targets(&request.platform)?;
    let boot = match (&request.kernel, &request.init) {
        (Some(kernel), Some(init)) => Boot::Given(Box::new(Config::new(
            PathBuf::from(kernel),
            Some(PathBuf::from(init)),
        ))),
        (None, None) => Boot::Stored(match crate::guest::current(home)? {
            Some(recorded) => recorded,
            None => crate::guest::default(home, say, Some(cancel), &|k| {
                shards_ipc::env_value(&request.registry_env, k)
            })?,
        }),
        _ => return Err("--kernel and --init (or SHARDS_KERNEL and SHARDS_INIT) go together".into()),
    };
    let (image, lease) = find_image(
        &request.image,
        request.pull,
        &targets,
        &request.registry_env,
        home,
        say,
        cancel,
    )?;
    // Its config as Docker reads it to run it (daemon/containerd GetImage): one Go reads
    // into no DockerOCIImage runs nothing.
    if let Some(e) = &image.config.run_error {
        return Err(format!("could not deserialize image config: {e}"));
    }
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
    let rootfs = image.rootfs.ok_or_else(|| {
        format!(
            "{}: an image of a platform this host's microVMs do not run",
            Reference::parse(&request.image).map_or_else(|_| request.image.clone(), |r| r.familiar())
        )
    })?;
    // An Agentfile's image: its Agentfile, as its root holds it and its digest names it
    // (D109). One whose Agentfile does not hold is not run.
    let agentfile = match image
        .config
        .config
        .as_ref()
        .and_then(|c| c.labels.as_ref())
        .and_then(|l| l.get("vnd.osi.agentfile.digest"))
    {
        Some(digest) => Some(crate::agentfile::agentfile(&rootfs, digest)?),
        None => None,
    };
    let image_labels = image
        .config
        .config
        .as_ref()
        .and_then(|c| c.labels.clone())
        .unwrap_or_default();
    let labels = merge_labels(image_labels, &request.labels)?;
    Ok(Prepared {
        agentfile,
        boot,
        rootfs,
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
        labels,
        size: (
            crate::resources::vcpus(&request.resources, crate::resources::host_cpus()),
            crate::resources::memory_mib(
                request.resources.memory,
                shards_vmm::vm::MEMORY_MIB,
                crate::build::host_memory(),
            ),
        ),
        image_volumes: image
            .config
            .config
            .as_ref()
            .map(|c| c.volumes.clone())
            .unwrap_or_default(),
        shares: 0,
        // The image's, then `--expose`'s, which `-P` publishes with them.
        exposed: {
            let mut exposed = image.config.config.map(|c| c.exposed_ports).unwrap_or_default();
            for e in &request.expose {
                if !exposed.contains(e) {
                    exposed.push(e.clone());
                }
            }
            exposed
        },
        image_id: image.resolved.to_string(),
    })
}

/// The template a run of `rootfs` on `guest` with `cfg`'s shape restores: named by the
/// SHA-256 of everything that goes into it, the snapshot format included.
pub fn template(home: &Path, guest: &Guest, rootfs: &Path, cfg: &Config) -> PathBuf {
    let key = format!(
        "snapshot format {}\nkernel {}\ninit {}\nrootfs {}\ncpus {}\nmemory {}\ncmdline {}\n{}",
        shards_vmm::snapshot::FORMAT,
        guest.kernel_digest,
        guest.init_digest,
        rootfs.display(),
        cfg.vcpus,
        cfg.memory_mib,
        cfg.cmdline,
        shares(cfg),
    ) + &pmem(cfg);
    let hex: String = Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    home.join("templates").join(hex)
}

/// A template's devices after its root filesystem, the in-VM server's among them (D60),
/// each named by content: the keys of those without stay as they were.
fn pmem(cfg: &Config) -> String {
    cfg.pmem
        .iter()
        .map(|p| format!("pmem {}\n", p.display()))
        .collect()
}

/// A template's shared directories (D38), in its key where it has any: the keys of those
/// without stay as they were.
fn shares(cfg: &Config) -> String {
    #[cfg(unix)]
    if !cfg.shares.is_empty() {
        return format!("shares {}\n", cfg.shares.len());
    }
    let _ = cfg;
    String::new()
}

/// What a template was saved from: its root filesystem and its guest. It is live while
/// that root filesystem is there and that guest is the current one (daemon.rs,
/// `collect_garbage`). It is the daemon's record, kept beside the template rather than in
/// it: a template's directory is a VM process's to write while it saves, and a VM taken
/// over then could leave there a record naming any file, or a link the daemon would write
/// through (PM M165).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Origin {
    rootfs: PathBuf,
    kernel_digest: String,
    init_digest: String,
    /// Whether it was saved with the in-VM server's device after its root filesystem (an
    /// Agentfile's image, D60), which a restore of it is then given too. Whether, not
    /// where: the device is the daemon's own (guest::server_device), never a path read
    /// from the template.
    #[serde(default)]
    server: bool,
}

impl Origin {
    /// The most bytes a record may take: a path and two digests, far less.
    const MOST: u64 = 64 << 10;

    /// Where the template `dir`'s record is: beside it, in the directory of templates,
    /// which no VM process is granted.
    fn place(dir: &Path) -> Option<(&Path, String)> {
        let parent = dir.parent()?;
        let name = dir.file_name()?.to_str()?;
        Some((parent, format!("{name}.origin")))
    }

    /// The template `dir`'s record, to remove with it.
    pub fn path(dir: &Path) -> Option<PathBuf> {
        Self::place(dir).map(|(parent, name)| parent.join(name))
    }

    pub fn of(guest: &Guest, rootfs: &Path, server: bool) -> Origin {
        Origin {
            rootfs: rootfs.to_path_buf(),
            kernel_digest: guest.kernel_digest.clone(),
            init_digest: guest.init_digest.clone(),
            server,
        }
    }

    /// Whether it was saved with the in-VM server's device.
    pub fn server(&self) -> bool {
        self.server
    }

    /// Records it for the template `dir`, beside it: written whole under a name of its
    /// own and renamed into place, so that nothing at its name is written through.
    pub fn write(&self, dir: &Path) -> Result<(), String> {
        let (parent, name) = Self::place(dir).ok_or_else(|| format!("{}: not a template", dir.display()))?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        let at = |e: std::io::Error| format!("{}: {e}", parent.join(&name).display());
        let templates = shards_vmm::platform::open_dir(parent).map_err(at)?;
        shards_vmm::platform::write_in(&templates, &name, &bytes).map_err(at)
    }

    /// What is recorded for the template `dir`, if it is recorded whole: a regular file
    /// beside it, never followed if it is a link.
    pub fn read(dir: &Path) -> Option<Origin> {
        let (parent, name) = Self::place(dir)?;
        let bytes = shards_vmm::platform::read_beneath(parent, &[&name], Self::MOST).ok()??;
        serde_json::from_slice(&bytes).ok()
    }

    /// The root filesystem the template was saved from.
    pub fn rootfs(&self) -> &Path {
        &self.rootfs
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
/// - The image's command applies only when no entrypoint is given, and no command. Its
///   entrypoint applies unless one is given; an empty one clears it, and then a command
///   must be given (moby daemon/commit.go merge; daemon/create.go: "no command specified").
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
        // Given, even empty, it leaves the image's command behind: only the run's follows.
        Some(given) => (given.clone(), asked.cmd.clone()),
        None => {
            let cmd = if asked.cmd.is_empty() {
                image.cmd.unwrap_or_default()
            } else {
                asked.cmd.clone()
            };
            (image.entrypoint.unwrap_or_default(), cmd)
        }
    };
    let argv: Vec<String> = entrypoint.into_iter().chain(cmd).collect();
    if argv.is_empty() {
        return Err("no command specified".into());
    }
    Ok(Options {
        argv,
        env,
        exec_env: Vec::new(),
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

    /// A template's origin is the daemon's record, beside the template where no VM
    /// process is granted (PM M165): it reads back as written; a record a VM could leave in
    /// the template itself is not read; a link at its place is replaced, never written
    /// through, nor read; a FIFO there is not read, nor waited on.
    #[cfg(unix)]
    #[test]
    fn a_templates_origin_is_beside_it_and_never_followed() {
        let home = std::env::temp_dir().join(format!("shards-origin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let templates = home.join("templates");
        let dir = templates.join("0abc");
        std::fs::create_dir_all(&dir).unwrap();
        let rootfs = home.join("rootfs.erofs");
        std::fs::write(&rootfs, b"").unwrap();
        let guest = Guest {
            kernel: home.join("kernel"),
            init: home.join("init"),
            kernel_digest: "sha256:k".into(),
            init_digest: "sha256:i".into(),
        };
        // What a VM taken over while it saved could leave in the template itself.
        std::fs::write(
            dir.join("origin.json"),
            br#"{"rootfs":"/etc/hosts","kernel_digest":"sha256:k","init_digest":"sha256:i","server":true}"#,
        )
        .unwrap();
        assert!(Origin::read(&dir).is_none(), "a record in the template is read");
        let place = Origin::path(&dir).unwrap();
        assert_eq!(place, templates.join("0abc.origin"));
        // A link at its place, to a file of the user's.
        let victim = home.join("victim");
        std::fs::write(&victim, b"the user's").unwrap();
        std::os::unix::fs::symlink(&victim, &place).unwrap();
        assert!(Origin::read(&dir).is_none(), "a link is read through");
        Origin::of(&guest, &rootfs, true).write(&dir).unwrap();
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"the user's",
            "written through a link"
        );
        let read = Origin::read(&dir).unwrap();
        assert_eq!((read.rootfs(), read.server()), (rootfs.as_path(), true));
        assert!(read.live(Some(&guest)));
        // A FIFO at its place: neither read nor waited on.
        std::fs::remove_file(&place).unwrap();
        let fifo = std::ffi::CString::new(place.clone().into_os_string().into_encoded_bytes()).unwrap();
        // SAFETY: mkfifo(2) of a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(Origin::read(&dir).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

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
            ..RunConfig::default()
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
        // An empty entrypoint clears the image's, and its command: one must be given
        // (measured: `docker run --entrypoint "" alpine` says "no command specified").
        let cleared = run(Run {
            entrypoint: Some(Vec::new()),
            cmd: strings(&["echo", "given"]),
            ..Run::default()
        });
        assert_eq!(cleared.argv, strings(&["echo", "given"]));
        assert_eq!(
            compose(
                Some(&image()),
                &Run {
                    entrypoint: Some(Vec::new()),
                    ..Run::default()
                }
            )
            .unwrap_err(),
            "no command specified"
        );
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

    /// A run's `--label` merges over the image's, but may not set a `vnd.osi.agentfile.*`
    /// label: that namespace is the build's record of the Agentfile, which the daemon and
    /// the guest trust (the user mutates a run through the CLI). The reserve is
    /// `vnd.osi.agentfile.`, not every label or every `vnd.osi.`.
    #[test]
    fn a_run_may_not_set_an_agentfile_label() {
        use std::collections::BTreeMap;
        let image = BTreeMap::from([
            ("vnd.osi.agentfile.digest".to_string(), "sha256:real".to_string()),
            ("org.opencontainers.image.title".to_string(), "img".to_string()),
        ]);
        // The run's own labels merge, including a bare `KEY` (empty value) and `vnd.osi.`
        // outside the reserved prefix.
        let merged = merge_labels(
            image.clone(),
            &strings(&["com.example=1", "bare", "vnd.osi.other=ok"]),
        )
        .unwrap();
        assert_eq!(merged.get("com.example").map(String::as_str), Some("1"));
        assert_eq!(merged.get("bare").map(String::as_str), Some(""));
        assert_eq!(merged.get("vnd.osi.other").map(String::as_str), Some("ok"));
        assert_eq!(
            merged.get("vnd.osi.agentfile.digest").map(String::as_str),
            Some("sha256:real")
        );
        // Setting one in the namespace is refused, whether it overwrites the image's or
        // adds a grant, and whatever its value (even empty, which would not remove it).
        for bad in [
            "vnd.osi.agentfile.digest=forged",
            "vnd.osi.agentfile.egress=1-65535",
            "vnd.osi.agentfile.digest=",
        ] {
            let e = merge_labels(image.clone(), &strings(&[bad])).unwrap_err();
            assert!(e.contains("is reserved"), "{bad}: {e}");
        }
    }
}
