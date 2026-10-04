//! Pulling an image as `docker pull` does (docs/design/architecture.md D19–D22) into this
//! user's image store: for `shards pull` and `shards run` (the daemon's), and what both
//! share of the store and its limits.

use std::io::Write;
use std::path::Path;

use shards_image::platform::Target;
use shards_image::reference::Reference;
use shards_image::store::{Limits, Store};

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
#[cfg(unix)]
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
