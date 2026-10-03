//! `shards pull IMAGE`: pulls an image as `docker pull` does (docs/design/architecture.md
//! D19–D22) into this user's image store, and says what it did as `docker pull` says it.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use shards_image::platform;
use shards_image::reference::Reference;
use shards_image::store::{Limits, Store};

use shards_registry::http::{Cancel, Client};
use shards_registry::pull::{self, Event, Pulled};
use shards_registry::registry::{self, Registry};
use shards_registry::{certs, credentials, tls};

const USAGE: &str = "usage: shards pull [-q] IMAGE
  Pulls IMAGE as `docker pull` does: with the credentials `docker login` left, the
  manifest for this host's guests, and every layer checked against its digests.
  -q, --quiet: print only the image's name.
  SHARDS_HOME: where images are kept, instead of shards in this user's data directory.";

pub fn pull(args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut quiet = false;
    let mut image = None;
    for arg in args {
        match arg.to_str() {
            Some("-q" | "--quiet") => quiet = true,
            Some("-h" | "--help") => {
                let _ = writeln!(std::io::stdout(), "{USAGE}");
                return ExitCode::SUCCESS;
            }
            Some(a) if !a.starts_with('-') && image.is_none() => image = Some(a.to_string()),
            _ => return usage(&format!("unexpected argument {arg:?}")),
        }
    }
    let Some(image) = image else {
        return usage("an image is required");
    };
    match run(&image, quiet) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}\n{USAGE}");
    ExitCode::from(2)
}

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

/// Pulls `image`, printing `docker pull`'s lines unless `quiet`.
pub fn run(image: &str, quiet: bool) -> Result<(), String> {
    let reference = Reference::parse(image).map_err(|e| e.to_string())?;
    let say = |line: &str| {
        if !quiet {
            let _ = writeln!(std::io::stdout(), "{line}");
        }
    };
    // `docker pull` names a default tag when the image named neither a tag nor a digest.
    let last = image.rsplit('/').next().unwrap_or_default();
    if !last.contains(':') && !image.contains('@') {
        say("Using default tag: latest");
    }
    let downloaded = std::sync::atomic::AtomicBool::new(false);
    let home = shards_ipc::home()?;
    let pulled = fetch(
        &home,
        &reference,
        &|event| match event {
            Event::Present(d) => say(&format!("{}: Already exists", short(&d.to_string()))),
            Event::Layer(d) => {
                downloaded.store(true, std::sync::atomic::Ordering::Relaxed);
                say(&format!("{}: Download complete", short(&d.to_string())));
            }
            Event::Manifest(..) | Event::Progress(..) | Event::Building => {}
        },
        &|line| say(line),
        None,
        &|k| std::env::var(k).ok(),
    )?;
    say(&format!("Digest: {}", pulled.0.resolved));
    // Docker's: up to date when the tag already named this image, or when a digest's
    // content was all here.
    let up_to_date =
        pulled.1 || (reference.digest.is_some() && !downloaded.load(std::sync::atomic::Ordering::Relaxed));
    let status = if up_to_date {
        "Image is up to date for"
    } else {
        "Downloaded newer image for"
    };
    say(&format!("Status: {status} {}", reference.familiar()));
    let _ = writeln!(std::io::stdout(), "{reference}");
    Ok(())
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
    };
    let mounts: Vec<String> = mount.into_iter().map(String::from).collect();
    Registry::for_push(http, reference, credentials, &mounts).map_err(|e| e.to_string())
}

/// Pulls `reference` into the store, until `cancel`, if given, is cancelled, with the
/// credentials and certificates `env` finds. Returns the pull, and whether the reference
/// already named the same manifest.
pub fn fetch(
    home: &Path,
    reference: &Reference,
    report: &(dyn Fn(Event<'_>) + Sync),
    say: &dyn Fn(&str),
    cancel: Option<&Cancel>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Pulled, bool), String> {
    let store = store(home)?;
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
    };
    let registry = Registry::new(http, reference, credentials).map_err(|e| e.to_string())?;
    let object = match &reference.digest {
        Some(d) => d.to_string(),
        None => reference.tag.clone().unwrap_or_else(|| "latest".into()),
    };
    say(&format!("{object}: Pulling from {}", reference.path));
    let before = store
        .tagged(&reference.to_string())
        .and_then(|d| d.map(|d| d.digest()).transpose())
        .map_err(|e| e.to_string())?;
    // Layers unpack without a cap, as Docker's do; each is checked against its DiffID.
    let limits = limits()?;
    let pulled = pull::pull(&registry, &store, reference, &platform::guest(), &limits, report)
        .map_err(|e| e.to_string())?;
    let same = before.as_ref() == Some(&pulled.manifest);
    // What the reference named before may be needed by nothing now: the daemon collects
    // it (daemon.rs, `collect_garbage`).
    if !same {
        let due = collect_due(home);
        std::fs::write(&due, b"").map_err(|e| format!("{}: {e}", due.display()))?;
    }
    Ok((pulled, same))
}

/// Docker's short layer ID: the digest's first 12 hex digits.
fn short(digest: &str) -> &str {
    let hex = digest.split_once(':').map_or(digest, |(_, h)| h);
    hex.get(..12).unwrap_or(hex)
}
