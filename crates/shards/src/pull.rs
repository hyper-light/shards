//! `shards pull IMAGE`: pulls an image as `docker pull` does (docs/design/architecture.md
//! D19–D22) into this user's image store, and says what it did as `docker pull` says it.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use shards_image::platform;
use shards_image::reference::Reference;
use shards_image::store::Store;
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

/// This user's image store, `images` in [`home`], readable by this user alone.
pub fn store() -> Result<Store, String> {
    let root = shards_ipc::home()?.join("images");
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
    let pulled = fetch(
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

/// Pulls `reference` into the store, until `cancel`, if given, is cancelled. Returns the
/// pull, and whether the reference already named the same manifest.
pub fn fetch(
    reference: &Reference,
    report: &(dyn Fn(Event<'_>) + Sync),
    say: &dyn Fn(&str),
    cancel: Option<&Cancel>,
) -> Result<(Pulled, bool), String> {
    let store = store()?;
    let env = |k: &str| std::env::var(k).ok();
    let (credentials, warnings) = credentials::lookup(&reference.domain, &env).map_err(|e| e.to_string())?;
    for warning in warnings {
        let _ = writeln!(std::io::stderr(), "WARNING: {warning}");
    }
    // One TLS configuration serves every host of the pull, as dockerd's per-registry client
    // does: the registry, its token realm and its CDN.
    let material = certs::load(registry::host(reference), &env).map_err(|e| e.to_string())?;
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
    let pulled = pull::pull(&registry, &store, reference, &platform::guest(), u64::MAX, report)
        .map_err(|e| e.to_string())?;
    let same = before.as_ref() == Some(&pulled.manifest);
    Ok((pulled, same))
}

/// Docker's short layer ID: the digest's first 12 hex digits.
fn short(digest: &str) -> &str {
    let hex = digest.split_once(':').map_or(digest, |(_, h)| h);
    hex.get(..12).unwrap_or(hex)
}
