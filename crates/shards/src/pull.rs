//! Pulling an image as `docker pull` does (docs/design/architecture.md D19–D22) into this
//! user's image store: for `shards pull` and `shards run` (the daemon's), and what both
//! share of the store and its limits.

use std::io::Write;
use std::path::Path;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_image::platform::{self as image_platform, Target};
use shards_image::reference::Reference;
use shards_image::store::{Limits, Store};
use shards_ipc::Progress;

use shards_registry::http::{Cancel, Client};
use shards_registry::proxy::Proxies;
use shards_registry::pull::{self, Event, Pulled};
use shards_registry::registry::{self, Registry};
use shards_registry::{certs, credentials, tls};

/// What building an image's root filesystem may take (audit A10; D18). By default nothing
/// but the machine bounds it, as nothing bounds containerd's unpacking or a BuildKit build
/// (containerd v2.3.6 core/diff/apply, pkg/archive; BuildKit v0.33.1 only collects its
/// cache after builds, cmd/buildkitd/config/gcpolicy.go): a full disk fails the write. Each
/// setting, a count of bytes or entries, sets a limit where an operator wants one:
/// - `SHARDS_MAX_IMAGE_BYTES`: what its layers decompress to, together.
/// - `SHARDS_MAX_IMAGE_ENTRIES` and `SHARDS_MAX_IMAGE_METADATA`: its entries, and the bytes
///   of their names, links and xattrs, all held in memory as it is built.
/// - `SHARDS_KEEP_FREE`: the bytes a build leaves free on the store's filesystem.
pub fn limits() -> Result<Limits, String> {
    let setting = |name: &str, default: u64| match std::env::var(name) {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("{name}: {v:?} is not a count")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(format!("{name}: {e}")),
    };
    Ok(Limits {
        bytes: setting("SHARDS_MAX_IMAGE_BYTES", u64::MAX)?,
        entries: setting("SHARDS_MAX_IMAGE_ENTRIES", u64::MAX)?,
        metadata: setting("SHARDS_MAX_IMAGE_METADATA", u64::MAX)?,
        keep_free: setting("SHARDS_KEEP_FREE", 0)?,
        available: |path| shards_vmm::platform::disk_space(path).map(|(available, _)| available),
    })
}

/// The file whose being there says a collection is due: a pull moved a reference.
pub fn collect_due(home: &Path) -> std::path::PathBuf {
    home.join("images").join("collect-due")
}

/// This user's image store, `images` in `home`, readable by this user alone.
pub fn store(home: &Path) -> Result<Store, String> {
    let root = home.join("images");
    shards_vmm::platform::create_private_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    Store::open(&root).map_err(|e| e.to_string())
}

/// Where `shards pull` says what it does: its lines, its errors, and, for a client on a
/// colour terminal, its steps instead of its lines ([`Progress`]).
pub struct Out<'a> {
    pub out: &'a (dyn Fn(&str) + Sync),
    pub err: &'a (dyn Fn(&str) + Sync),
    pub progress: Option<&'a (dyn Fn(&Progress) + Sync)>,
}

/// How often a client on a terminal hears how much of a layer has arrived: often enough
/// for motion at its frame rate, rarely enough that a 10 GB pull sends a few messages a
/// layer a second, not one for each read.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// `shards pull NAME[:TAG|@DIGEST]`, `-a` for every tag of the repository, as docker/cli
/// asks for it (cli/command/image/pull.go, runPull) and dockerd's containerd store pulls
/// (moby daemon/containerd/image_pull.go, PullImage): the reference read, `-a` checked
/// against a tag, the default tag said, the platform read; then each tag, or the one
/// named, said as `docker pull` says it; then the name. An image our microVMs run is made
/// ready to boot as it is pulled. The daemon runs it for its clients; a host without the
/// daemon runs it in the command's process. Returns the exit status.
pub fn command(
    parsed: &shards_cmdline::flags::Parsed,
    home: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    out: &Out<'_>,
    cancel: Option<&Cancel>,
) -> u8 {
    let refuse = |said: &str| {
        (out.err)(said);
        1
    };
    let given = parsed.args.first().map(String::as_str).unwrap_or_default();
    let (all, quiet) = (parsed.bool("all-tags"), parsed.bool("quiet"));
    // Its steps, where they are shown; `docker pull`'s lines otherwise.
    let show = out.progress.filter(|_| !quiet);
    let say = |line: &str| {
        if !quiet && show.is_none() {
            (out.out)(line);
        }
    };
    // What the CLI checks before it asks dockerd, in its words.
    let mut named = match Reference::parse_normalized(given) {
        Ok(r) => r,
        Err(e) => return refuse(&e.to_string()),
    };
    let name_only = named.tag.is_none() && named.digest.is_none();
    if all && !name_only {
        return refuse("tag can't be used with --all-tags/-a");
    }
    if !all && name_only {
        named.tag = Some("latest".into());
        say("Using default tag: latest");
    }
    let targets = match targets(parsed.string("platform")) {
        Ok(t) => t,
        Err(e) => return refuse(&e),
    };
    let status = (|| {
        let wanted = if all {
            let tags = registry(&named, cancel, env).and_then(|r| r.tags().map_err(|e| e.to_string()));
            match tags {
                Ok(tags) => tags
                    .into_iter()
                    .map(|t| Reference {
                        tag: Some(t),
                        ..named.clone()
                    })
                    .collect(),
                Err(e) => return refuse(&format!("Error response from daemon: {e}")),
            }
        } else {
            vec![named.clone()]
        };
        for reference in &wanted {
            let downloaded = AtomicBool::new(false);
            // Each layer's bytes so far, and when the client last heard of them.
            let arrived: Mutex<HashMap<String, (u64, Option<Instant>)>> = Mutex::new(HashMap::new());
            let report = |event: Event<'_>| {
                if let Event::Layer(_) = event {
                    downloaded.store(true, Ordering::Relaxed);
                }
                if let Some(progress) = show {
                    if let Some(p) = shown(&event, &arrived) {
                        progress(&p);
                    }
                    return;
                }
                match event {
                    Event::Present(d) => say(&format!("{}: Already exists", short(&d.to_string()))),
                    Event::Layer(d) => say(&format!("{}: Download complete", short(&d.to_string()))),
                    Event::Manifest(..) | Event::Progress(..) | Event::Building | Event::Pulling => {}
                }
            };
            let pulling = |line: &str| {
                if let Some(progress) = show {
                    progress(&Progress::Pulling {
                        reference: reference.to_string(),
                        repository: reference.path.clone(),
                    });
                } else {
                    say(line);
                }
            };
            let pulled = fetch(home, reference, &targets, &report, &pulling, cancel, env);
            let (pulled, same) = match pulled {
                Ok(p) => p,
                Err(e) if all => {
                    return refuse(&format!(
                        "Error response from daemon: error pulling {reference}: {e}"
                    ));
                }
                Err(e) => return refuse(&format!("Error response from daemon: {e}")),
            };
            // Docker's: up to date when the tag already named this image, or when a
            // digest's content was all here.
            let up_to_date = same || (reference.digest.is_some() && !downloaded.load(Ordering::Relaxed));
            if let Some(progress) = show {
                progress(&Progress::Done {
                    digest: pulled.resolved.to_string(),
                    unchanged: up_to_date,
                    bootable: pulled.rootfs.is_some(),
                });
            }
            say(&format!("Digest: {}", pulled.resolved));
            let status = if up_to_date {
                "Image is up to date for"
            } else {
                "Downloaded newer image for"
            };
            say(&format!("Status: {status} {}", reference.familiar()));
        }
        0
    })();
    if status == 0 {
        // The CLI's last line, said even when quiet: the name pulled, a tag's or the
        // repository's.
        (out.out)(&named.to_string());
    }
    status
}

/// A pull's event as a client on a terminal hears it, if it hears it now: a layer's
/// bytes at most every [`PROGRESS_EVERY`], summed in `arrived`.
fn shown(event: &Event<'_>, arrived: &Mutex<HashMap<String, (u64, Option<Instant>)>>) -> Option<Progress> {
    Some(match event {
        Event::Present(d) => Progress::Have(d.to_string()),
        Event::Layer(d) => Progress::Verified(d.to_string()),
        Event::Manifest(_, layers) => Progress::Layers(
            layers
                .iter()
                .map(|l| (l.digest.clone(), u64::try_from(l.size).unwrap_or(0)))
                .collect(),
        ),
        Event::Building => Progress::Building,
        Event::Pulling => return None,
        Event::Progress(d, n) => {
            let key = d.to_string();
            let mut held = arrived.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = held.entry(key.clone()).or_insert((0, None));
            entry.0 = entry.0.saturating_add(*n);
            let now = Instant::now();
            if entry
                .1
                .is_some_and(|last| now.duration_since(last) < PROGRESS_EVERY)
            {
                return None;
            }
            entry.1 = Some(now);
            Progress::Bytes(key, entry.0)
        }
    })
}

/// The platforms to pull for: `--platform`'s, read as containerd's `platforms.Parse`
/// reads it (v1.0.0-rc.5, which docker/cli v29.8.1 vendors), or else our guests'. A
/// specifier of an OS or an architecture alone takes the rest from our guests' platform,
/// where the CLI takes it from the machine it runs on: a client on macOS would otherwise
/// ask for `darwin` images, which no Linux daemon has.
fn targets(given: &str) -> Result<Vec<Target>, String> {
    if given.is_empty() {
        return Ok(image_platform::guest());
    }
    let guest = image_platform::guest();
    let ours = guest.first().ok_or("no platform for this host's guests")?;
    let build = shards_dockerfile::platform::Platform::new(&ours.os, &ours.architecture);
    let parsed = shards_dockerfile::platform::parse(given.as_bytes(), &build)
        .map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    Ok(vec![image_platform::normalize(&shards_image::oci::Platform {
        os: text(&parsed.os),
        architecture: text(&parsed.architecture),
        variant: Some(text(&parsed.variant)).filter(|v| !v.is_empty()),
        ..shards_image::oci::Platform::default()
    })])
}

/// A registry to push `reference`'s repository to, with the credentials and TLS a pull
/// of it would use, and pull access to `mount`, a repository of the same registry its
/// blobs may be mounted from.
#[cfg(unix)]
pub fn registry_for_push(
    reference: &Reference,
    mount: Option<&str>,
    cancel: Option<&Cancel>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Registry, String> {
    let (credentials, warnings) = credentials::lookup(&reference.domain, env).map_err(|e| e.to_string())?;
    for warning in warnings {
        let _ = writeln!(std::io::stderr(), "WARNING: {warning}");
    }
    let material = certs::load(registry::host(reference), env).map_err(|e| e.to_string())?;
    let config = tls::client_config(material.roots, material.client).map_err(|e| e.to_string())?;
    let http = Client::new(
        Box::new(move |_| Ok(config.clone())),
        &format!("shards/{}", env!("CARGO_PKG_VERSION")),
    );
    let http = match cancel {
        Some(cancel) => http.cancelled_by(cancel.clone()),
        None => http,
    }
    .with_proxies(Proxies::from_env(env));
    let mounts: Vec<String> = mount.into_iter().map(String::from).collect();
    Registry::for_push(http, reference, credentials, &mounts).map_err(|e| e.to_string())
}

/// Pulls `reference` into the store, until `cancel`, if given, is cancelled, with the
/// credentials and certificates `env` finds. Returns the pull, and whether the reference
/// already named the same manifest.
pub fn fetch(
    home: &Path,
    reference: &Reference,
    targets: &[Target],
    report: &(dyn Fn(Event<'_>) + Sync),
    say: &(dyn Fn(&str) + Sync),
    cancel: Option<&Cancel>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Pulled, bool), String> {
    let store = store(home)?;
    let registry = registry(reference, cancel, env)?;
    // dockerd names a digest's pull by the whole reference, a tag's by the tag.
    let object = match &reference.digest {
        Some(_) => reference.to_string(),
        None => reference.tag.clone().unwrap_or_else(|| "latest".into()),
    };
    // Said once the registry has answered for the reference, as dockerd says it.
    let report = |event: Event<'_>| match event {
        Event::Pulling => say(&format!("{object}: Pulling from {}", reference.path)),
        event => report(event),
    };
    let before = store
        .tagged(&reference.to_string())
        .and_then(|d| d.map(|d| d.digest()).transpose())
        .map_err(|e| e.to_string())?;
    // Layers unpack without a cap, as Docker's do; each is checked against its DiffID.
    let limits = limits()?;
    let pulled =
        pull::pull(&registry, &store, reference, targets, &limits, &report).map_err(|e| e.to_string())?;
    let same = before.as_ref() == Some(&pulled.manifest);
    // What the reference named before may be needed by nothing now: the daemon collects
    // it (daemon.rs, `collect_garbage`).
    if !same {
        let due = collect_due(home);
        std::fs::write(&due, b"").map_err(|e| format!("{}: {e}", due.display()))?;
    }
    Ok((pulled, same))
}

/// A registry to pull `reference`'s repository from, until `cancel`, if given, is
/// cancelled, with the credentials, certificates and proxies `env` finds.
pub fn registry(
    reference: &Reference,
    cancel: Option<&Cancel>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Registry, String> {
    let (credentials, warnings) = credentials::lookup(&reference.domain, env).map_err(|e| e.to_string())?;
    for warning in warnings {
        let _ = writeln!(std::io::stderr(), "WARNING: {warning}");
    }
    // One TLS configuration serves every host of the pull, as dockerd's per-registry client
    // does: the registry, its token realm and its CDN.
    let material = certs::load(registry::host(reference), env).map_err(|e| e.to_string())?;
    let config = tls::client_config(material.roots, material.client).map_err(|e| e.to_string())?;
    let http = Client::new(
        Box::new(move |_| Ok(config.clone())),
        &format!("shards/{}", env!("CARGO_PKG_VERSION")),
    );
    let http = match cancel {
        Some(cancel) => http.cancelled_by(cancel.clone()),
        None => http,
    }
    .with_proxies(Proxies::from_env(env));
    Registry::new(http, reference, credentials).map_err(|e| e.to_string())
}

/// Docker's short layer ID: the digest's first 12 hex digits.
pub(crate) fn short(digest: &str) -> &str {
    let hex = digest.split_once(':').map_or(digest, |(_, h)| h);
    hex.get(..12).unwrap_or(hex)
}

#[cfg(test)]
mod tests {
    /// The client sends every name a registry is reached by: its proxies' among them, so a
    /// daemon reaches registries through the proxies of the client that asks.
    #[test]
    fn clients_send_every_name_registries_are_reached_by() {
        for name in shards_registry::proxy::ENV {
            assert!(shards_ipc::REGISTRY_ENV.contains(&name), "{name}");
        }
    }
}
