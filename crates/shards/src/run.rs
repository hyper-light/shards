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

/// A host's VFIO group that a run's device names (D154), by `--device` or a CDI node:
/// `/dev/vfio/N`, or each group in `/dev/vfio` where it names the directory, as dockerd's
/// DevicesFromPath walks one; or a function of one by its VFIO device node,
/// `/dev/vfio/devices/vfioN`, or each in `/dev/vfio/devices`, its group's other functions
/// the host's.
#[derive(Debug, Clone)]
pub struct VfioGroup {
    /// Its path, `/dev/vfio/N` or `/dev/vfio/devices/vfioN`.
    pub path: String,
    /// Its functions the host gave vfio-pci, each given the run's microVM behind the
    /// microVM's own IOMMU, by which its guest gives them to the workload as a group of its
    /// own; one its guest's VFIO cannot give it otherwise.
    pub functions: Vec<shards_vmm::vm::Vfio>,
}

/// The functions `groups` give a microVM, in the order its bus places them, each once:
/// through its group where a run names the group too, whose node gives all of its
/// functions, as VFIO gives a group's functions one way at a time (vm::vfio_given), and
/// its guest still gives the workload each node it names.
pub fn vfio_functions(groups: &[VfioGroup]) -> Vec<shards_vmm::vm::Vfio> {
    let mut all: Vec<shards_vmm::vm::Vfio> = Vec::new();
    for f in groups.iter().flat_map(|g| &g.functions) {
        if !all.iter().any(|a| a.name == f.name) {
            all.push(f.clone());
        }
    }
    let named: Vec<std::path::PathBuf> = groups
        .iter()
        .flat_map(|g| &g.functions)
        .filter(|f| f.cdev.is_none())
        .map(|f| f.group.clone())
        .collect();
    for f in &mut all {
        if named.contains(&f.group) {
            f.cdev = None;
        }
    }
    all
}

/// The host's VFIO groups among the host paths `paths`, a run's devices' and its CDI
/// nodes' (D154): a path `/dev/vfio/N` the host has, or each such in `/dev/vfio` where it
/// names the directory, with the functions of IOMMU group N the host gave vfio-pci or another of
/// VFIO's PCI drivers (the group's PCI bridges, which VFIO passes through no guest, and
/// devices held by none, stay the host's); each group's members as sysfs lists them
/// (/sys/kernel/iommu_groups/N/devices). A path the host has no group at is no group: the
/// guest finds no device there either, and says so as dockerd does. A no-IOMMU group is
/// refused: its DMA reaches all of the host's memory, which no microVM is given.
pub fn vfio_groups<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<Vec<VfioGroup>, String> {
    let gathering = |host: &str, e: &str| {
        format!("error gathering device information while adding custom device {host:?}: {e}")
    };
    let group_name = |n: &str| {
        let number = n.strip_prefix("noiommu-").unwrap_or(n);
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
    };
    let cdev_name = |n: &str| {
        n.strip_prefix("vfio")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut groups: Vec<VfioGroup> = Vec::new();
    for host in paths {
        let clean = host.trim_end_matches('/');
        // A function by its device node: iommufd its DMA's (vfio.rst, "VFIO Device cdev").
        let cdevs: Vec<String> = if clean == "/dev/vfio/devices" {
            let mut names: Vec<String> = std::fs::read_dir(clean)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| cdev_name(n))
                .collect();
            names.sort_by_key(|n| (n.len(), n.clone()));
            names.into_iter().map(|n| format!("{clean}/{n}")).collect()
        } else if clean.strip_prefix("/dev/vfio/devices/").is_some_and(cdev_name) {
            vec![clean.to_string()]
        } else {
            Vec::new()
        };
        for cdev in &cdevs {
            if !Path::new(cdev).exists() {
                continue;
            }
            let f = shards_vmm::vm::Vfio::of_cdev(Path::new(cdev)).map_err(|e| gathering(host, &e))?;
            if !groups.iter().any(|g| g.path == *cdev) {
                groups.push(VfioGroup {
                    path: cdev.clone(),
                    functions: vec![shards_vmm::vm::Vfio { iommu: true, ..f }],
                });
            }
        }
        if !cdevs.is_empty() || clean.starts_with("/dev/vfio/devices") {
            continue;
        }
        let paths: Vec<String> = if clean == "/dev/vfio" {
            let Ok(entries) = std::fs::read_dir(clean) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| group_name(n))
                .collect();
            names.sort_by_key(|n| (n.len(), n.clone()));
            names.into_iter().map(|n| format!("/dev/vfio/{n}")).collect()
        } else if clean.strip_prefix("/dev/vfio/").is_some_and(group_name) {
            vec![clean.to_string()]
        } else {
            continue;
        };
        for path in paths {
            if !Path::new(&path).exists() {
                continue;
            }
            let name = path.strip_prefix("/dev/vfio/").unwrap_or_default();
            if name.starts_with("noiommu-") {
                return Err(gathering(
                    host,
                    &format!(
                        "{path} is a group of no IOMMU, whose DMA reaches all of the host's memory: a microVM is given none"
                    ),
                ));
            }
            let members = Path::new("/sys/kernel/iommu_groups").join(name).join("devices");
            let Ok(entries) = std::fs::read_dir(&members) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect();
            names.sort();
            let mut functions = Vec::with_capacity(names.len());
            for name in names {
                let driver = std::fs::read_link(format!("/sys/bus/pci/devices/{name}/driver")).ok();
                let vfio = driver
                    .as_ref()
                    .and_then(|d| d.file_name())
                    .and_then(|d| d.to_str())
                    .is_some_and(|d| d.ends_with("vfio-pci") || d.ends_with("vfio_pci"));
                if !vfio {
                    continue;
                }
                let f = shards_vmm::vm::Vfio::of(&name).map_err(|e| gathering(host, &e))?;
                if f.group != Path::new(&path) {
                    return Err(gathering(
                        host,
                        &format!("{name} is in {}, not this group", f.group.display()),
                    ));
                }
                functions.push(shards_vmm::vm::Vfio { iommu: true, ..f });
            }
            if functions.is_empty() {
                return Err(gathering(
                    host,
                    &format!("no function of {path} is bound to vfio-pci"),
                ));
            }
            if !groups.iter().any(|g| g.path == path) {
                groups.push(VfioGroup { path, functions });
            }
        }
    }
    Ok(groups)
}

/// What a run's CDI devices (`--device VENDOR/CLASS=NAME`) bring it, resolved as dockerd's
/// CDI driver resolves them as a container starts (moby daemon/cdi.go, injectCDIDevices;
/// CDI v1.1.0 Cache.InjectDevices): each name the device of that name in the specs of the
/// daemon's CDI directories (SHARDS_CDI_SPEC_DIRS, else /etc/cdi and /var/run/cdi), their
/// edits merged in CDI's order. What only a host can do is refused, as a build's step
/// refuses it (D96, D146).
pub fn cdi_edits(run: &Run) -> Result<Option<crate::build::cdi::Edits>, String> {
    use crate::build::cdi;
    if run.cdi_devices.is_empty() {
        return Ok(None);
    }
    let registry = cdi::Registry::load(&cdi::spec_dirs(&|k| std::env::var(k).ok()), &[]);
    let unresolved: Vec<&str> = run
        .cdi_devices
        .iter()
        .filter(|d| !registry.has(d))
        .map(String::as_str)
        .collect();
    if !unresolved.is_empty() {
        return Err(format!(
            "CDI device injection failed: unresolvable CDI devices {}",
            unresolved.join(", ")
        ));
    }
    let edits = registry.edits(&run.cdi_devices);
    if !edits.host_only.is_empty() {
        return Err(format!(
            "CDI device injection failed: CDI devices ask for what only a container's host can do, which a microVM has none of: {}",
            edits.host_only.join("; ")
        ));
    }
    Ok(Some(edits))
}

/// The render node of a guest's GPU, as Linux's virtio-gpu driver makes it for the first
/// (D144): a run or a step given it is given this Mac's GPU.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";

/// A run's GPU requests, each with how it was asked for: `--gpus`'s values as the CLI
/// reads them, then those an Engine API create gave (D170).
pub fn gpu_requests(run: &Run) -> Result<Vec<(String, shards_cmdline::gpus::GpuRequest)>, String> {
    let mut out = Vec::with_capacity(run.gpus.len() + run.device_requests.len());
    for value in &run.gpus {
        out.push((
            format!("--gpus {value}"),
            shards_cmdline::gpus::gpu_request(value)?,
        ));
    }
    for r in &run.device_requests {
        out.push((
            "HostConfig.DeviceRequests".to_string(),
            shards_cmdline::gpus::GpuRequest {
                driver: r.driver.clone(),
                count: r.count,
                device_ids: r.device_ids.clone(),
                capabilities: r.capabilities.clone(),
                options: r.options.iter().cloned().collect(),
            },
        ));
    }
    Ok(out)
}

/// Whether a run's GPU requests ask for this Mac's GPU (D144): a count other than none,
/// or the device `0`. It has one, which renders through Venus: more, another device, or
/// a driver of its own a request may not ask for, in dockerd's words where it finds no
/// driver for a request (daemon/devices.go, handleDevice).
pub fn wants_gpu(run: &Run) -> Result<bool, String> {
    let mut wants = false;
    for (asked, r) in gpu_requests(run)? {
        let caps = r
            .capabilities
            .iter()
            .map(|set| format!("[{}]", set.join(" ")))
            .collect::<Vec<_>>()
            .join(" ");
        if !r.driver.is_empty() || !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err(format!(
                "could not select device driver {:?} with capabilities: [{caps}]",
                r.driver
            ));
        }
        let devices = r.device_ids.as_deref().unwrap_or_default();
        if r.count > 1 || devices.iter().any(|d| d != "0") {
            return Err(format!(
                "this Mac has one GPU, device 0: {asked} asks for {}",
                if devices.is_empty() {
                    format!("{}", r.count)
                } else {
                    format!("device {}", devices.join(","))
                }
            ));
        }
        wants |= r.count != 0 || !devices.is_empty();
    }
    Ok(wants)
}

/// A request made ready to run.
pub struct Prepared {
    pub boot: Boot,
    /// The kernel its guest boots with this Mac's GPU, where `--gpus` asks for it and
    /// `boot` is the stored guest's (D144).
    pub gpu: Option<PathBuf>,
    /// The host's VFIO groups its `--device`s name (D154), or why they cannot be given it:
    /// its start fails then, and not its making, as dockerd finds a device as a container
    /// starts.
    pub vfio: Result<Vec<VfioGroup>, String>,
    /// The kernel its guest boots where it is given VFIO groups: the given one, or the
    /// pinned kernel's `kvm` variant.
    pub vfio_kernel: Option<PathBuf>,
    /// What its CDI devices bring it (D146), or why they cannot be: its start fails then,
    /// and not its making, as dockerd's resolves them as a container starts.
    pub cdi: Result<Option<crate::build::cdi::Edits>, String>,
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
    /// The slice of the user's service manager its microVM's processes are made under
    /// (D183, `--cgroup-parent`); empty for the daemon's own place.
    pub parent: String,
    /// The image's ID: what its reference resolved to.
    pub image_id: String,
    /// The microVM's vCPUs and the memory it boots with, in MiB ([`sizes`]).
    pub size: (u32, u64),
    /// The most memory the microVM may have in all, in MiB ([`sizes`], D148).
    pub memory_max: u64,
    /// The image's `VOLUME`s.
    pub image_volumes: Vec<String>,
    /// Its Agentfile, where it is an Agentfile's image (D109): what the daemon holds the
    /// run to, read in its root and checked against its digest, never from its labels.
    pub agentfile: Option<crate::agentfile::Agentfile>,
    /// How many directories it shares with its guest (D38): set once its mount points are.
    pub shares: u32,
    /// The emulator its image's programs run under, where it is another CPU's (D179).
    pub emulator: Option<Emulated>,
    /// What its writable layer holds, in bytes (D190): `--storage-opt size`, else
    /// [`LAYER_HOLDS`].
    pub layer: u64,
}

/// What a run's writable layer holds unless it asks for another size (D190,
/// `--storage-opt size`): Docker's device-mapper driver's default for a container's base
/// device, a block device's writable layer as a microVM's is (dockerd's `dm.basesize`,
/// 10G).
pub const LAYER_HOLDS: u64 = 10 << 30;

/// The emulator a run of another CPU's image runs under (D179): the emulator image's root
/// filesystem, one more of its microVM's devices, and the architecture its init registers
/// of it.
#[derive(Debug, Clone)]
pub struct Emulated {
    pub root: PathBuf,
    pub architecture: &'static str,
}

/// containerd's `platforms.FormatAll` of an image's platform as its config names it.
fn format_all(os: &str, architecture: &str, variant: &str) -> String {
    shards_image::platform::format_all(&shards_image::platform::Target {
        os: os.into(),
        architecture: architecture.into(),
        variant: variant.into(),
    })
}

/// The emulator a run of an image for `platform` runs under (D179), its CPU's, of the
/// emulator image, which is pulled as an image is the first time a run needs it, its pull
/// said in a line of its own: or why there is none.
fn emulator_for(
    platform: &shards_image::oci::Platform,
    request: &Run,
    home: &Path,
    say: &(dyn Fn(&str) + Sync),
    cancel: &Cancel,
) -> Result<Emulated, String> {
    let target = shards_image::platform::normalize(platform);
    let shown = format_all(&target.os, &target.architecture, &target.variant);
    let name = || Reference::parse(&request.image).map_or_else(|_| request.image.clone(), |r| r.familiar());
    // An amd64 level past the emulator's own runs nowhere here: its CPU lacks it, or the run
    // would be native (PM M251: the emulator has none of v4's AVX-512).
    if !shards_image::platform::emulator_runs(&target) {
        return Err(format!(
            "{}: an image of {shown}, built for CPUs with AVX-512, which neither this host's CPU nor its emulator has",
            name()
        ));
    }
    let emulator = (target.os == "linux")
        .then(|| shards_abi::emulate::of(&target.architecture))
        .flatten()
        .ok_or_else(|| {
            format!(
                "{}: an image of {shown}, which this host's microVMs neither run nor emulate",
                name()
            )
        })?;
    let store = crate::pull::store(home)?;
    let limits = crate::pull::limits()?;
    let reference = Reference::parse(crate::build::EMULATOR_IMAGE).map_err(|e| e.to_string())?;
    let guest = shards_image::platform::guest();
    let here = matches!(local(&store, &reference, &guest, &limits), Ok(Some(_)));
    if !here {
        say(&format!(
            "Downloading the emulator of {shown}'s programs, QEMU (tonistiigi/binfmt)"
        ));
    }
    // The run's own lease holds the store until its microVM has its devices: this one's
    // goes now.
    let (pulled, _lease) = find_image(
        crate::build::EMULATOR_IMAGE,
        Pull::Missing,
        &guest,
        &request.registry_env,
        home,
        &|_| {},
        cancel,
    )?;
    let root = pulled
        .rootfs
        .ok_or_else(|| format!("{}: no root filesystem of it here", crate::build::EMULATOR_IMAGE))?;
    Ok(Emulated {
        root,
        architecture: emulator.architecture,
    })
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
                // Here, but not for the platform asked for, as dockerd's lookup finds none
                // (errPlatformNotFound, a NotFound): pulled for it, where the run may pull.
                Err(e) if e.kind() == ErrorKind::NotFound && pull == Pull::Missing => None,
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
                // What the pull waits on (D184).
                Event::Waiting(notice) => say(notice),
                Event::Manifest(..)
                | Event::Progress(..)
                | Event::Building
                | Event::Unpacking(_)
                | Event::Pulling => {}
            };
            let (pulled, up_to_date) = crate::pull::fetch(
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
            // Up to date where the name already resolved to what it pulled: another
            // platform of an image here, or `--pull always` of one unchanged.
            let status = if up_to_date {
                "Image is up to date for"
            } else {
                "Downloaded newer image for"
            };
            say(&format!("Status: {status} {}", reference.familiar()));
            // A lone manifest is pulled whatever its platform, as dockerd pulls one; its run
            // for another platform is refused, as dockerd's lookup of that platform refuses
            // it.
            if let Some(asked) = targets.first()
                && !shards_image::platform::runs(&pulled.platform, targets)
            {
                return Err(
                    shards_registry::pull::not_for_platform(&reference.familiar(), asked).to_string(),
                );
            }
            pulled
        }
    };
    Ok((image, lease))
}

/// The image here `given` names by its ID (`sha256:` and 64 hex digits, or 4 to 64 of
/// them) where no image is tagged by that name, or by a name and the digest it resolved
/// to (`name@sha256:…`) where it was pulled by a tag, as dockerd's containerd store
/// resolves one (moby daemon/containerd/image.go, resolveImage). None where it names none:
/// the name is a reference, to find or pull.
fn by_id(
    store: &shards_image::store::Store,
    given: &str,
    targets: &[shards_image::platform::Target],
) -> Result<Option<shards_registry::pull::Pulled>, String> {
    use shards_image::reference::AnyReference;
    let parsed = AnyReference::parse(given);
    // A name with the digest it resolved to: a name to pull, as the CLI pulls one.
    let pullable = matches!(&parsed, Ok(AnyReference::Named(r)) if r.digest.is_some());
    let digest = pullable || matches!(parsed, Ok(AnyReference::Digest(_)));
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
    match shards_registry::pull::local_tagged(store, tagged, given, targets, &limits) {
        // Here, but not for the platform asked for: a name is pulled for it. An ID names
        // nothing to pull, and is refused in dockerd's words, which name the image found
        // (daemon/containerd/image.go, imageFamiliarName), where Docker's CLI would read a
        // short ID as a repository's name and pull that.
        Err(e) if e.kind() == ErrorKind::NotFound && pullable => Ok(None),
        Err(e) if e.kind() == ErrorKind::NotFound => match targets.first() {
            Some(asked) => {
                let name = match tagged.strip_prefix(shards_image::store::DANGLING) {
                    Some(digest) => digest.to_string(),
                    None => Reference::parse(tagged).map_or_else(|_| tagged.to_string(), |r| r.familiar()),
                };
                Err(shards_registry::pull::not_for_platform(&name, asked).to_string())
            }
            None => Err(e.to_string()),
        },
        found => found.map_err(|e| e.to_string()),
    }
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
        if shards_agentfile::agentfile::reserved_label(k.as_bytes()) {
            return Err(format!(
                "label {k:?} is reserved: shards' build sets the {} labels from the Agentfile, and a run may not set one",
                String::from_utf8_lossy(shards_agentfile::agentfile::LABEL_PREFIX)
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
    // What dockerd warns of at a create, whatever `-q` says.
    let warn = say;
    // `-q`: what a pull says goes unsaid (docker/cli create.go pullImage); errors do not.
    let quiet = |line: &str| {
        if !request.quiet {
            say(line);
        }
    };
    let say: &(dyn Fn(&str) + Sync) = &quiet;
    // `--platform`'s image; else, as dockerd takes one where a run names none, an index's
    // best for this host's microVMs, and a lone manifest or an image here whatever its
    // platform, another CPU's run under its emulator (D179).
    let targets = match request.platform.is_empty() {
        true => Vec::new(),
        false => crate::pull::targets(&request.platform)?,
    };
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
    let cdi = cdi_edits(request);
    let renders = cdi.as_ref().ok().and_then(Option::as_ref).is_some_and(|e| {
        let render = |p: &str| p == RENDER_NODE;
        e.nodes.iter().any(|n| render(&n.path) || render(&n.host_path))
    });
    let gpu = match (
        wants_gpu(request)? || (cfg!(target_os = "macos") && renders),
        &boot,
    ) {
        (false, _) => None,
        // The kernel given is the one it boots.
        (true, Boot::Given(cfg)) => Some(cfg.kernel.clone()),
        (true, Boot::Stored(_)) => Some(crate::guest::gpu_kernel(home, say, Some(cancel), &|k| {
            shards_ipc::env_value(&request.registry_env, k)
        })?),
    };
    // Its devices' and its CDI nodes' host paths, each a VFIO group's where it names one.
    let cdi_paths = cdi
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(|e| e.nodes.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|n| {
            if n.host_path.is_empty() {
                n.path.as_str()
            } else {
                n.host_path.as_str()
            }
        });
    let vfio = vfio_groups(
        request
            .devices
            .iter()
            .map(|d| d.split(':').next().unwrap_or_default())
            .chain(cdi_paths),
    );
    let vfio_kernel = match (&vfio, &boot) {
        (Ok(groups), _) if groups.is_empty() => None,
        (Err(_), _) => None,
        (Ok(_), Boot::Given(cfg)) => Some(cfg.kernel.clone()),
        (Ok(_), Boot::Stored(_)) => Some(crate::guest::kvm_kernel(home, say, Some(cancel), &|k| {
            shards_ipc::env_value(&request.registry_env, k)
        })?),
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
    // Its config, read as a run reads it (moby daemon/containerd GetImage): one that does
    // not read runs nothing.
    if let Some(e) = &image.config.run_error {
        return Err(format!("the image's config does not read: {e}"));
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
    // Another CPU's image (D179): one no guest runs itself runs under its CPU's emulator,
    // which its microVM registers as it boots; and a run that named no platform is told so,
    // in dockerd's words, whenever it is emulated. 32-bit Arm's among them on arm64, which
    // dockerd counts as the host's own, where no guest of shards' runs it (PM M250).
    let platform = shards_image::oci::Platform {
        os: image.config.os.clone(),
        architecture: image.config.architecture.clone(),
        variant: image.config.variant.clone(),
        ..shards_image::oci::Platform::default()
    };
    let native = shards_image::platform::runs(&platform, &shards_image::platform::guest());
    if request.platform.is_empty() && !native {
        let host = shards_image::platform::maximum();
        warn(&format!(
            "WARNING: The requested image's platform ({}) does not match the detected host platform ({}) and no specific platform was requested",
            format_all(
                &platform.os,
                &platform.architecture,
                platform.variant.as_deref().unwrap_or_default()
            ),
            format_all(&host.os, &host.architecture, &host.variant),
        ));
    }
    let emulator = match native {
        true => None,
        false => Some(emulator_for(&platform, request, home, say, cancel)?),
    };
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
        layer: crate::setup::storage_size(request)?.unwrap_or(LAYER_HOLDS),
        agentfile,
        emulator,
        boot,
        gpu,
        vfio,
        vfio_kernel,
        cdi,
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
        parent: request.cgroup_parent.clone(),
        size: {
            let (vcpus, boot, _) = sizes(&request.resources, crate::build::host_memory());
            (vcpus, boot)
        },
        memory_max: sizes(&request.resources, crate::build::host_memory()).2,
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

/// A run's microVM's vCPUs, every one of the host's (D191), the memory it boots with and
/// the most it may have in all, in MiB (D148). Every run boots as one without a limit does
/// and plugs memory as its workload asks: without a limit, up to the host's memory, as a
/// container without one may have; with one, up to what its limit needs, which its cgroup
/// holds it to (`memory.max`), never less than it boots with. So every limit's runs restore
/// one template, and its guest reports what it may plug (D149), by which runtimes size
/// their heaps as under Docker (Node.js's V8, PM M220, M221). Where the host's memory is
/// not known, it plugs none, and a run with a limit boots with what its limit needs.
pub fn sizes(resources: &shards_ipc::Resources, host: Option<u64>) -> (u32, u64, u64) {
    use shards_vmm::vm::MEMORY_MIB;
    let vcpus = crate::resources::microvm_vcpus();
    // Its swap's bookkeeping too, its kernel's and not its workload's (D159).
    let kernel = crate::resources::swap_bytes(resources).map_or(0, |allowance| {
        let device = crate::resources::swap_device_bytes(allowance, vcpus);
        crate::resources::swap_kernel_bytes(device.min(host.unwrap_or(u64::MAX)))
    });
    let limit = match resources.memory {
        m if m > 0 => m.saturating_add(i64::try_from(kernel).unwrap_or(i64::MAX)),
        m => m,
    };
    let needs = crate::resources::memory_mib(limit, MEMORY_MIB, host, vcpus);
    let boot = crate::resources::memory_mib(0, MEMORY_MIB, host, vcpus);
    match host {
        Some(bytes) if resources.memory <= 0 => (vcpus, boot, ((bytes >> 20) & !1).max(boot)),
        Some(_) => (vcpus, boot, needs.max(boot)),
        None => (vcpus, needs, needs),
    }
}

/// The bytes of each /dev/shm of a microVM, its own and each run's alike (D156): half of
/// `most_mib`, all the memory, in MiB, it may have, as Linux sizes a tmpfs by default
/// (Documentation/filesystems/tmpfs.rst). What a browser's renderers and a data loader's
/// workers share memory through, where Docker gives a container 64 MiB of its host's, which
/// Playwright's docs and PyTorch's README tell its users to raise. Its guest is told them
/// with each run (setup.rs, the `shm=` entry; a builder's on its kernel's command line,
/// `shards_shm`), and `inspect` says them.
pub fn shm_bytes(most_mib: u64) -> u64 {
    most_mib.saturating_mul(1 << 19)
}

/// The template a run of `rootfs` on `guest` with `cfg`'s shape and a writable layer that
/// holds `layer` bytes (D190) restores: named by the SHA-256 of everything that goes into
/// it, the snapshot format included.
pub fn template(home: &Path, guest: &Guest, rootfs: &Path, cfg: &Config, layer: u64) -> PathBuf {
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
    ) + &pmem(cfg)
        + &plug(cfg)
        + &swap(cfg)
        + gpu(cfg)
        + JOIN_SHARE
        + &share_window(cfg)
        + &format!("layer {layer}\n");
    let hex: String = Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    home.join("templates").join(hex)
}

/// Every template's join share (D119), which every microVM the daemon starts has, as it has
/// a join disk: in its key, so that none saved without one is restored as one with it.
const JOIN_SHARE: &str = "join share\n";

/// A GPU's template (D150), in its key where it has one.
fn gpu(cfg: &Config) -> &'static str {
    if cfg.gpu { "gpu\n" } else { "" }
}

/// A template's memory to plug (D148), in its key where it has any.
fn plug(cfg: &Config) -> String {
    match cfg.hotplug_mib {
        0 => String::new(),
        mib => format!("memory to plug {mib}\n"),
    }
}

/// A template's swap window (D159), in its key where it has one.
fn swap(cfg: &Config) -> String {
    match cfg.swap_mib {
        0 => String::new(),
        mib => format!("swap window {mib}\n"),
    }
}

/// A template's shares' DAX windows (D189), in its key where it has them.
fn share_window(cfg: &Config) -> String {
    match cfg.share_window_mib {
        0 => String::new(),
        mib => format!("share window {mib}\n"),
    }
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
/// that root filesystem is there, and its emulator's where it has one (D179), and that
/// guest is the current one (daemon.rs, `collect_garbage`). It is the daemon's record, kept beside the template rather than in
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
    /// Whether it is a GPU's (D150): saved by the GPU's VM process from the pinned GPU
    /// kernel, its restores the GPU's VM process's too.
    #[serde(default)]
    gpu: bool,
    /// The emulator image's root filesystem it was saved with after its other devices, where
    /// its image is another CPU's (D179), which a restore of it is then given too: the
    /// daemon's own record, as `rootfs` is.
    #[serde(default)]
    emulator: Option<PathBuf>,
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

    pub fn of(guest: &Guest, rootfs: &Path, server: bool, gpu: bool, emulator: Option<&Path>) -> Origin {
        Origin {
            rootfs: rootfs.to_path_buf(),
            kernel_digest: guest.kernel_digest.clone(),
            init_digest: guest.init_digest.clone(),
            server,
            gpu,
            emulator: emulator.map(Path::to_path_buf),
        }
    }

    /// The emulator image's root filesystem it was saved with (D179).
    pub fn emulator(&self) -> Option<&Path> {
        self.emulator.as_deref()
    }

    /// Whether it was saved with the in-VM server's device.
    pub fn server(&self) -> bool {
        self.server
    }

    /// Whether it is a GPU's (D150).
    pub fn gpu(&self) -> bool {
        self.gpu
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

    /// Whether it is still the template of `guest`'s runs: a GPU's is of the pinned GPU
    /// kernel instead of `guest`'s (D150).
    pub fn live(&self, guest: Option<&Guest>) -> bool {
        let gpu = || crate::kernel::KERNEL_GPU.map(|k| format!("sha256:{}", k.sha256));
        self.rootfs.is_file()
            && self.emulator.as_deref().is_none_or(Path::is_file)
            && guest.is_some_and(|g| {
                let kernel = if self.gpu {
                    gpu()
                } else {
                    Some(g.kernel_digest.clone())
                };
                kernel.as_deref() == Some(self.kernel_digest.as_str()) && g.init_digest == self.init_digest
            })
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
        terms: asked.terms.clone(),
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

    /// A microVM is given each function once, through its group where the run names the
    /// group as well as the function's device node, whichever it names first.
    #[test]
    fn a_group_named_too_gives_its_functions() {
        let f = |name: &str, cdev: Option<&str>| shards_vmm::vm::Vfio {
            name: name.into(),
            group: "/dev/vfio/7".into(),
            noiommu: false,
            bars64: Vec::new(),
            iommu: true,
            cdev: cdev.map(Into::into),
        };
        let node = |n: &str, function: &str| VfioGroup {
            path: format!("/dev/vfio/devices/vfio{n}"),
            functions: vec![f(function, Some(format!("/dev/vfio/devices/vfio{n}").as_str()))],
        };
        let group = VfioGroup {
            path: "/dev/vfio/7".into(),
            functions: vec![f("0000:01:00.0", None), f("0000:01:00.1", None)],
        };
        let by_group = vec![f("0000:01:00.1", None), f("0000:01:00.0", None)];
        assert_eq!(
            vfio_functions(&[node("1", "0000:01:00.1"), group.clone()]),
            by_group
        );
        assert_eq!(
            vfio_functions(&[group.clone(), node("1", "0000:01:00.1")]),
            vec![f("0000:01:00.0", None), f("0000:01:00.1", None)]
        );
        let nodes = [node("0", "0000:01:00.0"), node("1", "0000:01:00.1")];
        assert_eq!(
            vfio_functions(&nodes),
            vec![
                f("0000:01:00.0", Some("/dev/vfio/devices/vfio0")),
                f("0000:01:00.1", Some("/dev/vfio/devices/vfio1"))
            ]
        );
        assert_eq!(shards_vmm::vm::vfio_given(&vfio_functions(&nodes)), Ok(()));
        assert_eq!(
            shards_vmm::vm::vfio_given(&vfio_functions(&[node("1", "0000:01:00.1"), group])),
            Ok(())
        );
    }

    /// `--gpus` asks for this Mac's one GPU with any count but none, or its device 0; more,
    /// another device, or a driver of its own are refused, a driver in dockerd's words.
    #[test]
    fn gpus_ask_for_this_macs_one_gpu() {
        let wants = |v: &[&str]| {
            wants_gpu(&Run {
                gpus: v.iter().map(|s| s.to_string()).collect(),
                ..Run::default()
            })
        };
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(
                wants(&["all"]).unwrap_err(),
                "could not select device driver \"\" with capabilities: [[gpu]]"
            );
            return;
        }
        assert_eq!(wants(&[]), Ok(false));
        assert_eq!(wants(&["0"]), Ok(false));
        for asks in [&["all"][..], &["1"], &["device=0"], &["capabilities=compute"]] {
            assert_eq!(wants(asks), Ok(true), "{asks:?}");
        }
        assert_eq!(
            wants(&["2"]).unwrap_err(),
            "this Mac has one GPU, device 0: --gpus 2 asks for 2"
        );
        assert_eq!(
            wants(&["\"device=0,1\""]).unwrap_err(),
            "this Mac has one GPU, device 0: --gpus \"device=0,1\" asks for device 0,1"
        );
        assert_eq!(
            wants(&["driver=nvidia"]).unwrap_err(),
            "could not select device driver \"nvidia\" with capabilities: [[gpu]]"
        );
        // An Engine API create's, as it gave them (D170).
        let requests = |device_requests| {
            wants_gpu(&Run {
                device_requests,
                ..Run::default()
            })
        };
        let gpu = |count, ids: Option<&[&str]>| shards_ipc::DeviceRequest {
            count,
            device_ids: ids.map(|i| i.iter().map(|s| s.to_string()).collect()),
            capabilities: vec![vec!["gpu".into(), "utility".into()]],
            ..shards_ipc::DeviceRequest::default()
        };
        assert_eq!(requests(vec![gpu(-1, None)]), Ok(true));
        assert_eq!(requests(vec![gpu(0, Some(&["0"]))]), Ok(true));
        assert_eq!(requests(vec![gpu(0, None)]), Ok(false));
        assert_eq!(
            requests(vec![gpu(2, None)]).unwrap_err(),
            "this Mac has one GPU, device 0: HostConfig.DeviceRequests asks for 2"
        );
    }

    /// A template's origin is the daemon's record, beside the template where no VM
    /// process is granted (PM M165): it reads back as written; a record a VM could leave in
    /// the template itself is not read; a link at its place is replaced, never written
    /// through, nor read; a FIFO there is not read, nor waited on.
    #[cfg(unix)]
    #[test]
    fn a_templates_origin_is_beside_it_and_never_followed() {
        let home_dir = shards_testdir::TempDir::new("origin").unwrap();
        let home = home_dir.join("origin");
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
        Origin::of(&guest, &rootfs, true, false, None)
            .write(&dir)
            .unwrap();
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
    fn health_checks_merge_with_the_images() {
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
    fn images_and_command_lines_merge() {
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
