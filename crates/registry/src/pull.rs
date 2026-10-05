//! A pull, as `docker pull` makes one (docs/research/registry-pull.md R1, R6):
//! 1. resolve the reference;
//! 2. choose the manifest for our guests' platform;
//! 3. fetch the config and check its layers;
//! 4. fetch the layers, over as many connections as pay (fetch.rs);
//! 5. build the image's root filesystem and record the reference.
//!
//! Nothing counts as pulled until every size and digest, and every layer's DiffID, has
//! been checked. An image found in the store again is checked as its pull checked it.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex, PoisonError};

use shards_image::oci::{self, Descriptor, Document, ImageConfig, Manifest, Platform};
use shards_image::platform::{self, Target};
use shards_image::reference::{Digest, Reference};
use shards_image::store::{Held, Layer, Limits, Store};

use crate::registry::Registry;
use crate::{Error, ErrorKind};

/// The image configs a runnable image has (image-spec config.md; Docker's schema 2).
const CONFIGS: [&str; 2] = [
    "application/vnd.oci.image.config.v1+json",
    "application/vnd.docker.container.image.v1+json",
];

/// A pulled image.
#[derive(Debug)]
pub struct Pulled {
    /// What the reference resolved to: an index, or the manifest itself.
    pub resolved: Digest,
    /// The manifest for our guests' platform.
    pub manifest: Digest,
    /// Its config's digest: the image's ID.
    pub id: Digest,
    pub config: ImageConfig,
    /// The image's EROFS root filesystem, built when our guests run its platform: an
    /// image of another is stored for `push` and `save` alone.
    pub rootfs: Option<PathBuf>,
    /// Its layers' count and compressed bytes.
    pub layers: usize,
    pub compressed: u64,
    /// The platforms its index offers, `os/arch[/variant]`; none for a lone manifest.
    pub platforms: Vec<String>,
    /// The attestations kept with it: provenance and SBOMs.
    pub attestations: usize,
}

/// What a pull reports as it goes.
#[derive(Debug)]
pub enum Event<'a> {
    /// The manifest for our guests' platform, and its layers.
    Manifest(&'a Digest, &'a [Descriptor]),
    /// A layer already stored.
    Present(&'a Digest),
    /// Bytes of a layer arrived.
    Progress(&'a Digest, u64),
    /// A layer is stored and verified.
    Layer(&'a Digest),
    /// The root filesystem is being built.
    Building,
    /// The layer at this index is being unpacked into it.
    Unpacking(usize),
    /// The manifest for our platform is known, and about to be fetched: where dockerd
    /// says it is pulling (daemon/containerd/image_pull.go, on the first manifest).
    Pulling,
}

/// Pulls `reference` for the platforms `targets` into `store`, its root filesystem built
/// within `limits`. Fails as dockerd reports a pull's failure.
pub fn pull(
    registry: &Registry,
    store: &Store,
    reference: &Reference,
    targets: &[Target],
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
) -> Result<Pulled, Error> {
    pull_as(registry, store, reference, targets, limits, report, false)
}

/// [`pull`], every layer fetched again though stored, and the root filesystem built
/// again in its place: `shards pull --no-cache`, which `docker pull` has not.
pub fn pull_again(
    registry: &Registry,
    store: &Store,
    reference: &Reference,
    targets: &[Target],
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
) -> Result<Pulled, Error> {
    pull_as(registry, store, reference, targets, limits, report, true)
}

fn pull_as(
    registry: &Registry,
    store: &Store,
    reference: &Reference,
    targets: &[Target],
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
    fresh: bool,
) -> Result<Pulled, Error> {
    pulled(registry, store, reference, targets, limits, report, fresh).map_err(|e| {
        // daemon/containerd/image_pull.go: a refused authorization in dockerd's own
        // words, but for want of basic credentials, which it leaves containerd's.
        if e.kind() == ErrorKind::Unauthorized && !e.to_string().contains("no basic auth credentials") {
            let mut bare = reference.clone();
            bare.tag = None;
            bare.digest = None;
            return Error::of(
                ErrorKind::Unauthorized,
                format!(
                    "pull access denied for {}, repository does not exist or may require 'docker login'",
                    bare.familiar()
                ),
            );
        }
        e.in_dockerds_words()
    })
}

fn pulled(
    registry: &Registry,
    store: &Store,
    reference: &Reference,
    targets: &[Target],
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
    fresh: bool,
) -> Result<Pulled, Error> {
    let name = reference.familiar();
    // What it writes is recorded only at its end: no collection runs meanwhile.
    let _lease = store.lease()?;
    let top = registry
        .resolve(store, reference)
        .map_err(|e| resolving(e, reference))?;
    let resolved = top.digest()?;
    // What the name resolved to, as the registry described it.
    let target = Descriptor {
        platform: None,
        annotations: Default::default(),
        ..top.clone()
    };
    // The attestations of the manifest chosen, as dockerd keeps them: provenance and SBOMs.
    let mut attestations: Vec<Descriptor> = Vec::new();
    let mut platforms: Vec<String> = Vec::new();
    let pulling = std::sync::atomic::AtomicBool::new(false);
    let say_pulling = || {
        if !pulling.swap(true, Ordering::Relaxed) {
            report(Event::Pulling);
        }
    };
    if matches!(
        top.media_type.as_str(),
        oci::media::OCI_MANIFEST | oci::media::DOCKER_MANIFEST
    ) {
        say_pulling();
    }
    let (manifest_desc, manifest) = match document(registry, store, &top)? {
        Document::Manifest(m) => {
            say_pulling();
            (top, m)
        }
        Document::Index(index) => {
            // containerd's words (LimitManifests), after dockerd's.
            let chosen = platform::select(&index, targets).ok_or_else(|| {
                Error::of(
                    ErrorKind::NotFound,
                    format!(
                        "no matching manifest for {} in the manifest list entries: no match for platform in manifest: not found",
                        pulling_for(targets)
                    ),
                )
            })?;
            platforms = index
                .manifests
                .iter()
                .filter_map(|d| d.platform.as_ref())
                .filter(|p| p.os != "unknown")
                .map(|p| match &p.variant {
                    Some(v) if !v.is_empty() => format!("{}/{}/{v}", p.os, p.architecture),
                    _ => format!("{}/{}", p.os, p.architecture),
                })
                .collect();
            attestations = index
                .manifests
                .iter()
                .filter(|d| {
                    d.annotations.get(ATTESTATION_TYPE).map(String::as_str) == Some(ATTESTATION)
                        && d.annotations.get(ATTESTATION_FOR) == Some(&chosen.digest)
                })
                .cloned()
                .collect();
            say_pulling();
            match document(registry, store, chosen)? {
                Document::Manifest(m) => (chosen.clone(), m),
                Document::Index(_) => {
                    return Err(Error::new(format!("{name}: an index inside an index")));
                }
            }
        }
    };
    let manifest_digest = manifest_desc.digest()?;
    contents(&name, &manifest)?;
    // Layers compress what they hold: more of them than the image may decompress to is
    // refused before anything is downloaded (audit A10).
    let compressed = manifest
        .layers
        .iter()
        .try_fold(0u64, |n, l| l.size().map(|s| n.saturating_add(s)))?;
    if compressed > limits.bytes {
        return Err(Error::new(format!(
            "{name}: its layers are {compressed} bytes, more than the {} it may take (SHARDS_MAX_IMAGE_BYTES)",
            limits.bytes
        )));
    }
    report(Event::Manifest(&manifest_digest, &manifest.layers));

    if fresh {
        registry.fetch_blob_again(store, &manifest.config, limits, &|_| {})?;
    } else {
        registry.fetch_blob(store, &manifest.config, limits, &|_| {})?;
    }
    // A stored config that has changed is fetched again in its place.
    let config = match stored(store, &name, &manifest.config, oci::MAX_CONFIG) {
        Err(e) if e.kind() == ErrorKind::Changed => {
            registry.fetch_blob_again(store, &manifest.config, limits, &|_| {})?;
            stored(store, &name, &manifest.config, oci::MAX_CONFIG)?
        }
        read => read?,
    };
    let (config, layers) = checked(&name, &manifest_desc, &manifest, &config, targets)?;

    // The attestations come as the layers download and the image is built, each a few
    // small documents a round trip apart: not after it all.
    let (rootfs, attested) = std::thread::scope(|scope| {
        let attest = || {
            let mut fetched = Vec::new();
            for attestation in &attestations {
                fetched.extend(fetch_attestation(registry, store, &name, attestation, limits)?);
            }
            Ok::<_, Error>(fetched)
        };
        let attesting = std::thread::Builder::new()
            .name("shards-attest".into())
            .spawn_scoped(scope, attest);
        let ours = platform::runs(
            &oci::Platform {
                os: config.os.clone(),
                architecture: config.architecture.clone(),
                variant: config.variant.clone(),
                ..oci::Platform::default()
            },
            &platform::guest(),
        );
        let built = if ours {
            build(registry, store, &manifest, &layers, limits, report, fresh).map(Some)
        } else {
            crate::fetch::layers(registry, store, &manifest, limits, report, &|_| {}, fresh).map(|()| None)
        };
        let attested = match attesting {
            Ok(thread) => thread
                .join()
                .unwrap_or_else(|_| Err(Error::new("fetching the attestations failed"))),
            // No thread to spare: fetched here.
            Err(_) => attest(),
        };
        (built, attested)
    });
    let rootfs = rootfs?;
    let id = manifest.config.digest()?;
    let mut contents = vec![manifest_digest.clone(), id.clone()];
    contents.extend(layers.iter().map(|l| l.blob.clone()));
    contents.extend(attested?);
    store.tag_from(
        &reference.to_string(),
        &manifest_desc,
        &target,
        &contents,
        Some(&reference.name()),
    )?;
    Ok(Pulled {
        resolved,
        manifest: manifest_digest,
        id,
        config,
        rootfs,
        layers: manifest.layers.len(),
        compressed,
        platforms,
        attestations: attestations.len(),
    })
}

/// Downloads the layers `manifest` names that are not here, and builds the image's root
/// filesystem from `layers` as they come: each layer is unpacked once its blob is stored,
/// while the rest download, so that a pull takes what the longer of the two does, not
/// both (PM M114).
fn build(
    registry: &Registry,
    store: &Store,
    manifest: &Manifest,
    layers: &[Layer],
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
    fresh: bool,
) -> Result<PathBuf, Error> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Blob {
        Coming,
        Here,
        Never,
    }
    let blobs = Mutex::new(vec![Blob::Coming; layers.len()]);
    let arrived = Condvar::new();
    let held = || blobs.lock().unwrap_or_else(PoisonError::into_inner);
    let here = |i: usize| {
        if let Some(b) = held().get_mut(i) {
            *b = Blob::Here;
        }
        arrived.notify_all();
    };
    let ready = |i: usize| -> Result<(), shards_image::Error> {
        let mut blobs = held();
        loop {
            match blobs.get(i) {
                Some(Blob::Here) => {
                    drop(blobs);
                    report(Event::Unpacking(i));
                    return Ok(());
                }
                Some(Blob::Coming) => blobs = arrived.wait(blobs).unwrap_or_else(PoisonError::into_inner),
                Some(Blob::Never) | None => {
                    return Err(std::io::Error::other("a layer was not fetched").into());
                }
            }
        }
    };
    report(Event::Building);
    let rootfs = |ready: &(dyn Fn(usize) -> Result<(), shards_image::Error> + Sync)| {
        if fresh {
            store.rootfs_again_as_ready(layers, limits, ready)
        } else {
            store.rootfs_as_ready(layers, limits, ready)
        }
    };
    let (fetched, built) = std::thread::scope(|scope| {
        let building = std::thread::Builder::new()
            .name("shards-rootfs".into())
            .spawn_scoped(scope, || rootfs(&ready));
        let fetched = crate::fetch::layers(registry, store, manifest, limits, report, &here, fresh);
        // What will not come now: the build stops waiting for it.
        for b in held().iter_mut().filter(|b| **b == Blob::Coming) {
            *b = Blob::Never;
        }
        arrived.notify_all();
        let built: Result<PathBuf, Error> = match building {
            Ok(thread) => thread
                .join()
                .map_err(|_| Error::new("building the root filesystem failed"))
                .and_then(|r| r.map_err(Error::from)),
            // No thread to spare: built here, now that the layers are.
            Err(_) => rootfs(&ready).map_err(Error::from),
        };
        (fetched, built)
    });
    fetched?;
    match built {
        Ok(rootfs) => Ok(rootfs),
        Err(e) => {
            // A layer stored before that has changed since fails its DiffID, and would on
            // every pull: each such is fetched again in its place, and the image built
            // once more. containerd takes what it stored as it stands.
            let mut mended = false;
            for layer in &manifest.layers {
                let digest = layer.digest()?;
                if !store.intact(&digest)? {
                    registry
                        .fetch_blob_again(store, layer, limits, &|n| report(Event::Progress(&digest, n)))?;
                    mended = true;
                }
            }
            if !mended {
                return Err(e);
            }
            Ok(store.rootfs(layers, limits)?)
        }
    }
}

/// A resolve's failure, as containerd's client reports it: after `failed to resolve
/// reference`, and a refused authorization after its resolver's words for one.
fn resolving(e: Error, reference: &Reference) -> Error {
    let e = if e.kind() == ErrorKind::Unauthorized {
        e.context("pull access denied, repository does not exist or may require authorization")
    } else {
        e
    };
    e.context(format!("failed to resolve reference \"{reference}\""))
}

/// The platform dockerd names where an index has none for it: the one it pulls for,
/// containerd's `DefaultSpec` as `FormatAll` writes it. That is our first target, with
/// arm64's variant as Linux reports every AArch64 CPU's (`CPU architecture: 8`): v8,
/// which `Normalize` folds away.
fn pulling_for(targets: &[Target]) -> String {
    match targets.first() {
        Some(t) if t.architecture == "arm64" && t.variant.is_empty() => format!("{}/arm64/v8", t.os),
        Some(t) if t.variant.is_empty() => format!("{}/{}", t.os, t.architecture),
        Some(t) => format!("{}/{}/{}", t.os, t.architecture, t.variant),
        None => String::new(),
    }
}

/// An index's tell for a manifest that attests to another (BuildKit's attestations).
const ATTESTATION_TYPE: &str = "vnd.docker.reference.type";
const ATTESTATION: &str = "attestation-manifest";
const ATTESTATION_FOR: &str = "vnd.docker.reference.digest";

/// Fetches an attestation manifest, its config and layers, kept as they are: what it
/// fetched, by digest. Its layers are statements, never unpacked.
fn fetch_attestation(
    registry: &Registry,
    store: &Store,
    name: &str,
    desc: &Descriptor,
    limits: &Limits,
) -> Result<Vec<Digest>, Error> {
    let Document::Manifest(attestation) = document(registry, store, desc)? else {
        return Err(Error::new(format!("{name}: an attestation that is an index")));
    };
    let mut fetched = vec![desc.digest()?];
    for part in std::iter::once(&attestation.config).chain(&attestation.layers) {
        let digest = part.digest()?;
        if !store.has(&digest) {
            registry.fetch_blob(store, part, limits, &|_| {})?;
        }
        fetched.push(digest);
    }
    Ok(fetched)
}

/// The image `reference` names, if it has been pulled for one of `targets`: from the
/// store alone, and checked as its pull checked it. Its root filesystem is built again if
/// it has gone.
pub fn local(
    store: &Store,
    reference: &Reference,
    targets: &[Target],
    limits: &Limits,
) -> Result<Option<Pulled>, Error> {
    // A root filesystem it builds again is its reference's: no collection runs meanwhile.
    let _lease = store.lease()?;
    let Some(manifest_desc) = store.tagged(&reference.to_string())? else {
        return Ok(None);
    };
    let resolved = store
        .resolved(&reference.to_string())?
        .unwrap_or(manifest_desc.digest()?);
    unpack(
        store,
        &reference.familiar(),
        &manifest_desc,
        resolved,
        targets,
        limits,
    )
    .map(Some)
}

/// The image whose manifest for one of `targets` `manifest_desc` describes, here in the
/// store, checked as a pull checks it, and its root filesystem, built if it is not:
/// what `local` finds, and what `load` makes of an archive. `name` is for messages;
/// `resolved` is what its name resolved to. The caller holds the store's lease.
pub fn unpack(
    store: &Store,
    name: &str,
    manifest_desc: &Descriptor,
    resolved: Digest,
    targets: &[Target],
    limits: &Limits,
) -> Result<Pulled, Error> {
    let manifest_digest = manifest_desc.digest()?;
    let bytes = stored(store, name, manifest_desc, oci::MAX_MANIFEST)?;
    let Document::Manifest(manifest) = oci::parse_document(&bytes, &manifest_desc.media_type)? else {
        return Err(Error::new(format!("{name}: its record names an index")));
    };
    // A shards microVM (save::microvm): its disk is its root filesystem, nothing to
    // unpack; what runs is in the image config it carries.
    if manifest.artifact_type.as_deref() == Some(shards_image::save::MICROVM) {
        return microvm(store, name, manifest_desc, &manifest, resolved, targets);
    }
    contents(name, &manifest)?;
    let config = stored(store, name, &manifest.config, oci::MAX_CONFIG)?;
    let (config, layers) = checked(name, manifest_desc, &manifest, &config, targets)?;
    let rootfs = store.rootfs(&layers, limits)?;
    let compressed = manifest
        .layers
        .iter()
        .try_fold(0u64, |n, l| l.size().map(|s| n.saturating_add(s)))?;
    Ok(Pulled {
        resolved,
        manifest: manifest_digest,
        id: manifest.config.digest()?,
        config,
        rootfs: Some(rootfs),
        layers: manifest.layers.len(),
        compressed,
        platforms: Vec::new(),
        attestations: 0,
    })
}

/// A stored shards microVM: its disk, checked against its digest's file being there and
/// its size, and the image config it was made from.
fn microvm(
    store: &Store,
    name: &str,
    manifest_desc: &Descriptor,
    manifest: &Manifest,
    resolved: Digest,
    targets: &[Target],
) -> Result<Pulled, Error> {
    use shards_image::save::{MICROVM_CONFIG, MICROVM_DISK};
    let layer = |media: &str| {
        manifest
            .layers
            .iter()
            .find(|l| l.media_type == media)
            .ok_or_else(|| Error::new(format!("{name}: a microVM without its {media}")))
    };
    let (disk, config_desc) = (layer(MICROVM_DISK)?, layer(MICROVM_CONFIG)?);
    let bytes = stored(store, name, config_desc, oci::MAX_CONFIG)?;
    let config = oci::parse_config(&bytes)?;
    let platform = manifest_desc.platform.clone().unwrap_or_else(|| Platform {
        architecture: config.architecture.clone(),
        os: config.os.clone(),
        variant: config.variant.clone(),
        os_features: Vec::new(),
    });
    if !platform::runs(&platform, targets) {
        return Err(Error::new(format!(
            "{name} is for {}/{}, not {}",
            platform.os,
            platform.architecture,
            wanted(targets)
        )));
    }
    let path = store.blob_path(&disk.digest()?);
    let size = std::fs::metadata(&path).map(|m| m.len()).map_err(|e| {
        Error::of(
            ErrorKind::Changed,
            format!("{name}: its disk {}: {e}", disk.digest),
        )
    })?;
    if size != disk.size()? {
        return Err(Error::of(
            ErrorKind::Changed,
            format!("{name}: its disk is {size} bytes, not {}", disk.digest),
        ));
    }
    Ok(Pulled {
        resolved,
        manifest: manifest_desc.digest()?,
        id: config_desc.digest()?,
        config,
        rootfs: Some(path),
        layers: 1,
        compressed: size,
        platforms: Vec::new(),
        attestations: 0,
    })
}

/// What a manifest names, checked before any of it is read: the config of a runnable
/// image, small enough to read whole, and layers of types we unpack.
fn contents(name: &str, manifest: &Manifest) -> Result<(), Error> {
    if !CONFIGS.contains(&manifest.config.media_type.as_str()) {
        return Err(Error::new(format!(
            "{name}: not a container image: its config is {:?}",
            manifest.config.media_type
        )));
    }
    let size = manifest.config.size()?;
    if size > oci::MAX_CONFIG {
        return Err(Error::new(format!(
            "{name}: its {size}-byte config is over the {}-byte limit",
            oci::MAX_CONFIG
        )));
    }
    for layer in &manifest.layers {
        oci::layer_compression(&layer.media_type)?;
    }
    Ok(())
}

/// The config and layers of the image `manifest` describes, checked, where `chosen` is
/// the descriptor the manifest was chosen by and `config` its config's checked bytes. The
/// image must be for one of `targets`: by the platform an index labelled it with, or by
/// its config's own when nothing labelled it, as containerd checks it. Its config must
/// describe layers, one DiffID for each of the manifest's (image-spec config.md).
fn checked(
    name: &str,
    chosen: &Descriptor,
    manifest: &Manifest,
    config: &[u8],
    targets: &[Target],
) -> Result<(ImageConfig, Vec<Layer>), Error> {
    let config = oci::parse_config(config)?;
    let platform = chosen.platform.clone().unwrap_or_else(|| Platform {
        architecture: config.architecture.clone(),
        os: config.os.clone(),
        variant: config.variant.clone(),
        os_features: Vec::new(),
    });
    if !platform::runs(&platform, targets) {
        return Err(Error::new(format!(
            "{name} is for {}/{}, not {}",
            platform.os,
            platform.architecture,
            wanted(targets)
        )));
    }
    if config.rootfs.diff_ids.len() != manifest.layers.len() {
        return Err(Error::new(format!(
            "{name}: {} layers but {} DiffIDs",
            manifest.layers.len(),
            config.rootfs.diff_ids.len()
        )));
    }
    let layers = manifest
        .layers
        .iter()
        .zip(&config.rootfs.diff_ids)
        .map(|(d, id)| {
            Ok(Layer {
                blob: d.digest()?,
                media_type: d.media_type.clone(),
                diff_id: Digest::parse(id)?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok((config, layers))
}

/// The bytes of the small blob `desc` describes, from the store alone, checked again
/// against its digest and size (`Store::held`). A copy that has changed is an error of its
/// own kind, [`ErrorKind::Changed`], which a pull mends.
fn stored(store: &Store, name: &str, desc: &Descriptor, max: u64) -> Result<Vec<u8>, Error> {
    match store.held(desc, max)? {
        Held::Whole(bytes) => Ok(bytes),
        Held::Changed(why) => Err(Error::of(ErrorKind::Changed, why)),
        Held::Invalid(why) => Err(Error::new(why)),
        Held::Missing => Err(Error::new(format!("{name}: {} is not in the store", desc.digest))),
    }
}

/// An index or manifest, fetched and parsed by its descriptor's media type.
fn document(registry: &Registry, store: &Store, desc: &Descriptor) -> Result<Document, Error> {
    let bytes = registry.fetch_document(store, desc)?;
    Ok(oci::parse_document(&bytes, &desc.media_type)?)
}

/// The platforms wanted, for messages: `linux/arm64`.
fn wanted(targets: &[Target]) -> String {
    targets
        .iter()
        .map(|t| {
            if t.variant.is_empty() {
                format!("{}/{}", t.os, t.architecture)
            } else {
                format!("{}/{}/{}", t.os, t.architecture, t.variant)
            }
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize};

    use sha2::{Digest as _, Sha256};

    use crate::auth::Credentials;
    use crate::http::Client;
    use crate::testing::{After, Seen, Server, route};

    const TOKEN: &str = "secret-token";

    fn sha256(bytes: &[u8]) -> String {
        Digest::from_hash(shards_image::reference::Algorithm::Sha256, &Sha256::digest(bytes)).to_string()
    }

    /// A tar holding one regular file, as ustar writes it.
    fn tar(name: &str, data: &[u8]) -> Vec<u8> {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[148..156].copy_from_slice(b"        ");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut out = h.to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512 + 1024, 0);
        out
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// A multi-platform image: its tagged index, manifests and blobs by digest.
    struct Image {
        manifests: HashMap<String, (Vec<u8>, String)>,
        blobs: HashMap<String, Vec<u8>>,
        layers: Vec<String>,
    }

    fn descriptor(media_type: &str, bytes: &[u8]) -> String {
        format!(
            r#"{{"mediaType":"{media_type}","digest":"{}","size":{}}}"#,
            sha256(bytes),
            bytes.len()
        )
    }

    /// An image for `arch` (and one for s390x beside it), with `files` as its layers.
    fn image(arch: &str, files: &[(&str, &[u8])], diff_ids_right: bool) -> Image {
        labelled(arch, arch, files, diff_ids_right)
    }

    /// An image an index labels for `arch`, whose config says `config_arch`.
    fn labelled(arch: &str, config_arch: &str, files: &[(&str, &[u8])], diff_ids_right: bool) -> Image {
        let mut blobs = HashMap::new();
        let mut layer_descs = Vec::new();
        let mut diff_ids = Vec::new();
        let mut layers = Vec::new();
        for (name, data) in files {
            let t = tar(name, data);
            let gz = gzip(&t);
            diff_ids.push(if diff_ids_right {
                sha256(&t)
            } else {
                sha256(b"wrong")
            });
            layer_descs.push(descriptor(&format!("{}+gzip", oci::media::OCI_LAYER), &gz));
            layers.push(sha256(&gz));
            blobs.insert(sha256(&gz), gz);
        }
        let config = format!(
            r#"{{"architecture":"{config_arch}","os":"linux","config":{{"Cmd":["/bin/sh"]}},"rootfs":{{"type":"layers","diff_ids":[{}]}}}}"#,
            diff_ids.iter().map(|d| format!("\"{d}\"")).collect::<Vec<_>>().join(",")
        )
        .into_bytes();
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{},"layers":[{}]}}"#,
            oci::media::OCI_MANIFEST,
            descriptor(CONFIGS[0], &config),
            layer_descs.join(",")
        )
        .into_bytes();
        blobs.insert(sha256(&config), config);
        let other = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":2},"layers":[]}"#.to_vec();
        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{},{}]}}"#,
            oci::media::OCI_INDEX,
            descriptor(oci::media::OCI_MANIFEST, &other)
                .replace('}', r#","platform":{"architecture":"s390x","os":"linux"}}"#),
            descriptor(oci::media::OCI_MANIFEST, &manifest).replace(
                '}',
                &format!(r#","platform":{{"architecture":"{arch}","os":"linux"}}}}"#)
            ),
        )
        .into_bytes();
        let mut manifests = HashMap::new();
        for (doc, media_type) in [
            (index, oci::media::OCI_INDEX),
            (manifest, oci::media::OCI_MANIFEST),
            (other, oci::media::OCI_MANIFEST),
        ] {
            manifests.insert(sha256(&doc), (doc.clone(), media_type.to_string()));
            if media_type == oci::media::OCI_INDEX {
                manifests.insert("v1".into(), (doc, media_type.to_string()));
            }
        }
        Image {
            manifests,
            blobs,
            layers,
        }
    }

    fn http(status: &str, fields: &[(&str, String)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n");
        for (n, v) in fields {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut out = out.into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// A registry serving `image` as `test/image` behind a bearer token, and the CDN it
    /// redirects blob requests to, on another origin. `cut` is a blob whose first download
    /// from the CDN stops halfway.
    struct Fake {
        registry: Server,
        cdn: Server,
    }

    fn fake(image: Image, cut: Option<String>) -> Fake {
        fake_throttling(image, cut, 0, None)
    }

    /// What a fake's CDN waits on before it serves a blob: its digest, and a condition.
    type Hold = (String, Box<dyn Fn() -> bool + Send + Sync>);

    /// [`fake`], whose CDN holds `hold`'s blob until its condition holds (or 10 s pass).
    fn fake_holding(image: Image, hold: Hold) -> Fake {
        fake_with(image, None, 0, None, Some(hold))
    }

    fn fake_throttling(
        image: Image,
        cut: Option<String>,
        throttles: usize,
        retry_after: Option<&'static str>,
    ) -> Fake {
        fake_with(image, cut, throttles, retry_after, None)
    }

    /// [`fake`], whose registry first answers `throttles` of its `/v2/` requests with a
    /// 429, and `retry_after` if there is one.
    fn fake_with(
        image: Image,
        cut: Option<String>,
        throttles: usize,
        retry_after: Option<&'static str>,
        hold: Option<Hold>,
    ) -> Fake {
        let throttled = AtomicUsize::new(0);
        let blobs = image.blobs.clone();
        let cut_done = AtomicBool::new(false);
        let cdn = route(None, move |req: &Seen| {
            // The CDN is another origin: it must never see the registry's token.
            if req.header("authorization").is_some() {
                return Some((http("400 Bad Request", &[], b"authorization leaked"), After::Keep));
            }
            let digest = req.target.strip_prefix("/cdn/").unwrap_or_default();
            let Some(bytes) = blobs.get(digest) else {
                return Some((http("404 Not Found", &[], b""), After::Keep));
            };
            if let Some((held, until)) = &hold
                && held == digest
            {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !until() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            if cut.as_deref() == Some(digest) && !cut_done.swap(true, Ordering::SeqCst) {
                let mut head = http("200 OK", &[], bytes);
                head.truncate(head.len() - bytes.len() / 2);
                return Some((head, After::Close));
            }
            // `bytes=from-` or `bytes=from-last`, as a CDN serves them.
            let range = req.header("range").and_then(|r| {
                let (from, last) = r.strip_prefix("bytes=")?.split_once('-')?;
                let from: usize = from.parse().ok()?;
                let last: usize = if last.is_empty() {
                    bytes.len() - 1
                } else {
                    last.parse().ok()?
                };
                Some((from, last.min(bytes.len() - 1)))
            });
            if let Some((from, last)) = range {
                let range = format!("bytes {from}-{last}/{}", bytes.len());
                let partial = http(
                    "206 Partial Content",
                    &[("Content-Range", range)],
                    &bytes[from..=last],
                );
                return Some((partial, After::Keep));
            }
            Some((http("200 OK", &[], bytes), After::Keep))
        });
        let cdn_port = cdn.port;
        let port = Arc::new(AtomicU16::new(0));
        let own = port.clone();
        let registry = route(None, move |req: &Seen| {
            let path = req.target.split('?').next().unwrap_or_default();
            if path.starts_with("/v2/") && throttled.fetch_add(1, Ordering::SeqCst) < throttles {
                let fields: Vec<(&str, String)> = retry_after
                    .map(|a| ("Retry-After", a.to_string()))
                    .into_iter()
                    .collect();
                return Some((http("429 Too Many Requests", &fields, b""), After::Keep));
            }
            if path == "/token" {
                let token = format!(r#"{{"token":"{TOKEN}","expires_in":300}}"#);
                return Some((http("200 OK", &[], token.as_bytes()), After::Keep));
            }
            if req.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
                let challenge = format!(
                    r#"Bearer realm="http://127.0.0.1:{}/token",service="fake",scope="repository:test/image:pull""#,
                    own.load(Ordering::SeqCst)
                );
                return Some((
                    http("401 Unauthorized", &[("WWW-Authenticate", challenge)], b""),
                    After::Keep,
                ));
            }
            if let Some(reference) = path.strip_prefix("/v2/test/image/manifests/") {
                let Some((doc, media_type)) = image.manifests.get(reference) else {
                    return Some((http("404 Not Found", &[], b""), After::Keep));
                };
                let fields = [
                    ("Content-Type", media_type.clone()),
                    ("Docker-Content-Digest", sha256(doc)),
                ];
                let mut response = http("200 OK", &fields, doc);
                if req.method == "HEAD" {
                    response.truncate(response.len() - doc.len());
                }
                return Some((response, After::Keep));
            }
            if let Some(digest) = path.strip_prefix("/v2/test/image/blobs/") {
                let location = format!("http://127.0.0.1:{cdn_port}/cdn/{digest}");
                return Some((
                    http("307 Temporary Redirect", &[("Location", location)], b""),
                    After::Keep,
                ));
            }
            Some((http("404 Not Found", &[], b""), After::Keep))
        });
        port.store(registry.port, Ordering::SeqCst);
        Fake { registry, cdn }
    }

    fn arm64() -> Vec<Target> {
        vec![Target {
            os: "linux".into(),
            architecture: "arm64".into(),
            variant: String::new(),
        }]
    }

    fn riscv64() -> Vec<Target> {
        vec![Target {
            os: "linux".into(),
            architecture: "riscv64".into(),
            variant: String::new(),
        }]
    }

    /// A directory of its own, removed when dropped, whether its test passes or panics.
    struct Temp(std::path::PathBuf);

    impl std::ops::Deref for Temp {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl AsRef<std::path::Path> for Temp {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> Temp {
        let dir = std::env::temp_dir().join(format!("shards-pull-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }

    fn client() -> Client {
        Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        )
    }

    fn count(server: &Server, prefix: &str) -> usize {
        server.requests().iter().filter(|r| r.starts_with(prefix)).count()
    }

    #[test]
    fn images_pull_through_tokens_redirects_and_a_cut_download() {
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let image = image("arm64", &[("etc/hostname", b"box\n"), ("data/big", &big)], true);
        let cut = image.layers[1].clone();
        let fake = fake(image, Some(cut.clone()));
        let server = &fake.registry;
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let root = temp("full");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let events = Mutex::new(Vec::new());
        let pulled = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|e| {
            let event = match e {
                // As dockerd says it: the index fetched, our manifest not yet.
                Event::Pulling => format!(
                    "pulling after {} GET",
                    count(server, "GET /v2/test/image/manifests/")
                ),
                Event::Manifest(..) => "manifest".to_string(),
                Event::Layer(_) => "layer".to_string(),
                _ => return,
            };
            events.lock().unwrap().push(event);
        })
        .unwrap();
        let image_bytes = std::fs::read(pulled.rootfs.as_ref().unwrap()).unwrap();
        assert_eq!(
            u32::from_le_bytes(image_bytes[1024..1028].try_into().unwrap()),
            0xE0F5_E1E2
        );
        // The record keeps the descriptor the index chose the manifest by.
        let recorded = store.tagged(&reference.to_string()).unwrap().unwrap();
        assert_eq!(recorded.digest().unwrap(), pulled.manifest);
        assert_eq!(
            recorded.platform.map(|p| p.architecture),
            Some("arm64".to_string())
        );
        assert_eq!(
            pulled.config.config.unwrap().cmd,
            Some(vec!["/bin/sh".to_string()])
        );
        assert_eq!(
            *events.lock().unwrap(),
            ["pulling after 1 GET", "manifest", "layer", "layer"]
        );
        // One token; the tag resolved by HEAD; the index and our manifest each fetched once.
        assert_eq!(count(server, "GET /token"), 1);
        assert_eq!(
            count(server, "HEAD /v2/test/image/manifests/v1 "),
            2,
            "unauthorized, then with the token"
        );
        assert_eq!(count(server, "GET /v2/test/image/manifests/"), 2);
        // The cut download resumed from where it stopped, on the CDN, without the token.
        let resumed = fake
            .cdn
            .requests()
            .into_iter()
            .filter(|r| r.starts_with(&format!("GET /cdn/{cut}")))
            .collect::<Vec<_>>();
        assert_eq!(resumed.len(), 2, "{resumed:?}");
        assert!(
            resumed[1].to_ascii_lowercase().contains("\r\nrange: bytes="),
            "{}",
            resumed[1]
        );
        assert!(
            fake.cdn
                .requests()
                .iter()
                .all(|r| !r.to_ascii_lowercase().contains("\r\nauthorization:"))
        );

        // Pulling again finds everything stored: it only resolves the tag.
        let before = server.requests().len();
        pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap();
        let again: Vec<String> = server.requests()[before..].to_vec();
        assert!(
            again
                .iter()
                .all(|r| r.starts_with("HEAD /v2/test/image/manifests/v1 ")),
            "{again:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn platforms_our_guests_cannot_run_are_refused() {
        let arm = fake(image("arm64", &[("a", b"a")], true), None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", arm.registry.port)).unwrap();
        let root = temp("platform");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let e = pull(
            &registry,
            &store,
            &reference,
            &riscv64(),
            &Limits::none(),
            &|_| {},
        )
        .unwrap_err();
        // dockerd's words, then containerd's (LimitManifests).
        assert_eq!(e.kind(), ErrorKind::NotFound);
        assert_eq!(
            e.to_string(),
            "no matching manifest for linux/riscv64 in the manifest list entries: no match for platform in manifest: not found"
        );
        // arm64 named as containerd's DefaultSpec names it on Linux: with its variant.
        let amd64 = fake(image("amd64", &[("a", b"a")], true), None);
        let reference =
            Reference::parse(&format!("127.0.0.1:{}/test/image:v1", amd64.registry.port)).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let e = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap_err();
        assert!(
            e.to_string()
                .starts_with("no matching manifest for linux/arm64/v8 in the manifest list entries"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn layers_whose_diff_ids_disagree_are_refused() {
        let fake = fake(image("arm64", &[("a", b"a")], false), None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let root = temp("diffid");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let e = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap_err();
        assert!(e.to_string().contains("DiffID"), "{e}");
        assert_eq!(
            store.tagged(&reference.to_string()).unwrap(),
            None,
            "nothing recorded"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pull's failures as dockerd reports them: the first four as Docker 29.3.1 does
    /// against registry:2 with basic authentication, measured; the rest as its source
    /// and containerd v2.4.1's words them (translateRegistryError, withGETErrorBody,
    /// invalidAuthorization).
    #[test]
    fn failures_are_reported_as_dockerd_reports_them() {
        let tokens = route(None, |_| {
            Some((
                http(
                    "200 OK",
                    &[("Content-Type", "application/json".into())],
                    br#"{"token":"t"}"#,
                ),
                After::Keep,
            ))
        });
        let realm = format!(
            r#"Bearer realm="http://127.0.0.1:{}/token",service="s""#,
            tokens.port
        );
        let basic = || ("WWW-Authenticate", r#"Basic realm="r""#.to_string());
        let denied = br#"{"errors":[{"code":"DENIED","message":"requested access to the resource is denied","detail":"quota"}]}"#;
        let password = || Credentials::Password {
            username: "u".into(),
            password: "wrong".into(),
        };
        type Answer = Box<dyn Fn(&Seen) -> Vec<u8> + Send + Sync>;
        let cases: Vec<(Credentials, Answer, &str, ErrorKind)> = vec![
            (
                Credentials::Anonymous,
                Box::new(move |_| http("401 Unauthorized", &[basic()], b"")),
                r#"failed to resolve reference "{R}": pull access denied, repository does not exist or may require authorization: authorization failed: no basic auth credentials"#,
                ErrorKind::Unauthorized,
            ),
            (
                password(),
                Box::new(move |_| http("401 Unauthorized", &[basic()], b"")),
                r#"unknown: failed to resolve reference "{R}": unexpected status from HEAD request to {U}: 401 Unauthorized"#,
                ErrorKind::Other,
            ),
            (
                Credentials::Anonymous,
                Box::new(|_| http("404 Not Found", &[], b"")),
                r#"failed to resolve reference "{R}": {R}: not found"#,
                ErrorKind::NotFound,
            ),
            (
                Credentials::Anonymous,
                Box::new(move |seen| {
                    let challenge = match seen.header("authorization") {
                        Some(_) => format!(r#"{realm},error="insufficient_scope""#),
                        None => realm.clone(),
                    };
                    http("401 Unauthorized", &[("WWW-Authenticate", challenge)], b"")
                }),
                "pull access denied for {N}, repository does not exist or may require 'docker login'",
                ErrorKind::Unauthorized,
            ),
            (
                Credentials::Anonymous,
                Box::new(move |seen| match seen.method.as_str() {
                    "HEAD" => http("403 Forbidden", &[], b""),
                    _ => http("403 Forbidden", &[], denied),
                }),
                "error from registry: requested access to the resource is denied - quota",
                ErrorKind::Other,
            ),
            (
                Credentials::Anonymous,
                Box::new(move |seen| match seen.method.as_str() {
                    "HEAD" => http("403 Forbidden", &[], b""),
                    _ => http("400 Bad Request", &[], denied),
                }),
                r#"unknown: failed to resolve reference "{R}": unexpected status from HEAD request to {U}: 403 Forbidden"#,
                ErrorKind::Other,
            ),
        ];
        let root = temp("dockerd-says");
        let store = Store::open(&root).unwrap();
        for (credentials, answer, expected, kind) in cases {
            let server = route(None, move |seen| Some((answer(seen), After::Keep)));
            let name = format!("127.0.0.1:{}/test/image", server.port);
            let reference = Reference::parse(&format!("{name}:v1")).unwrap();
            let registry = Registry::new(client(), &reference, credentials).unwrap();
            // Nothing is said to be pulling where the reference did not resolve.
            let pulling = AtomicBool::new(false);
            let e = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|e| {
                if matches!(e, Event::Pulling) {
                    pulling.store(true, Ordering::SeqCst);
                }
            })
            .unwrap_err();
            assert!(!pulling.load(Ordering::SeqCst), "{expected}");
            let expected = expected
                .replace("{R}", &format!("{name}:v1"))
                .replace("{N}", &name)
                .replace(
                    "{U}",
                    &format!("http://127.0.0.1:{}/v2/test/image/manifests/v1", server.port),
                );
            assert_eq!(e.to_string(), expected);
            assert_eq!(e.kind(), kind, "{expected}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A manifest at the tag itself is said to be pulled before it is fetched, as dockerd
    /// says it of the first manifest it sees.
    #[test]
    fn a_tagged_manifest_is_said_to_be_pulled_before_it_is_fetched() {
        let mut image = image("arm64", &[("a", b"a")], true);
        let index: serde_json::Value = serde_json::from_slice(&image.manifests["v1"].0).unwrap();
        let arm = index["manifests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["platform"]["architecture"] == "arm64")
            .unwrap()["digest"]
            .as_str()
            .unwrap()
            .to_string();
        let manifest = image.manifests[&arm].clone();
        image.manifests.insert("single".into(), manifest);
        let fake = fake(image, None);
        let server = &fake.registry;
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:single", server.port)).unwrap();
        let root = temp("single");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let said = Mutex::new(Vec::new());
        pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|e| {
            if matches!(e, Event::Pulling) {
                said.lock()
                    .unwrap()
                    .push(count(server, "GET /v2/test/image/manifests/"));
            }
        })
        .unwrap();
        assert_eq!(*said.lock().unwrap(), [0]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rate_limits_are_reported_and_not_retried() {
        let server = route(None, |_| {
            let fields = [
                ("ratelimit-limit", "100;w=21600".to_string()),
                ("ratelimit-remaining", "0;w=21600".to_string()),
                ("docker-ratelimit-source", "192.0.2.1".to_string()),
                ("Retry-After", "3600".to_string()),
            ];
            Some((http("429 Too Many Requests", &fields, b""), After::Keep))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let root = temp("ratelimit");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let e = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "unknown: failed to resolve reference \"{reference}\": unexpected status from HEAD request to \
                 http://127.0.0.1:{}/v2/test/image/manifests/v1: 429 Too Many Requests \
                 (limit 100 per 21600 s; 0 left; counted for 192.0.2.1; retry after 3600)",
                server.port
            )
        );
        assert_eq!(server.requests().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A layer is unpacked as soon as it is stored, while the rest download: the CDN holds
    /// the last layer until the first's decompressed archive is in the store's ingest
    /// directory, which a pull that built only once all were here would never reach.
    #[test]
    fn layers_unpack_while_the_rest_download() {
        let image = image(
            "arm64",
            &[("a", b"the first layer"), ("b", b"the last layer")],
            true,
        );
        let (first, last) = (image.layers[0].clone(), image.layers[1].clone());
        let mut first_tar = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::MultiGzDecoder::new(&image.blobs[&first][..]),
            &mut first_tar,
        )
        .unwrap();
        let root = temp("pipelined");
        let store = Store::open(&root).unwrap();
        let ingest = root.join("ingest");
        let saw = Arc::new(AtomicBool::new(false));
        let seen = saw.clone();
        let fake = fake_holding(
            image,
            (
                last,
                Box::new(move || {
                    let unpacked = std::fs::read_dir(&ingest)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .any(|e| std::fs::read(e.path()).is_ok_and(|b| b == first_tar));
                    if unpacked {
                        seen.store(true, Ordering::SeqCst);
                    }
                    unpacked
                }),
            ),
        );
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let pulled = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap();
        assert!(pulled.rootfs.is_some());
        assert!(
            saw.load(Ordering::SeqCst),
            "the first layer was not unpacked while the last downloaded"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A throttle is waited out, as ECR Public's is (PM M112); a `Retry-After` past the
    /// waits left is not.
    #[test]
    fn throttles_are_waited_out_and_long_waits_are_not() {
        for (retry_after, pulled) in [(None, true), (Some("1"), true), (Some("3600"), false)] {
            let image = image("arm64", &[("a", b"a")], true);
            let fake = fake_throttling(image, None, 2, retry_after);
            let reference =
                Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
            let root = temp("throttled");
            let store = Store::open(&root).unwrap();
            let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
            let got = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {});
            assert_eq!(got.is_ok(), pulled, "{retry_after:?}: {got:?}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn content_that_does_not_match_its_digest_is_refused() {
        let mut image = image("arm64", &[("a", b"a")], true);
        let layer = image.layers[0].clone();
        image.blobs.get_mut(&layer).unwrap()[10] ^= 0xff;
        let fake = fake(image, None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let root = temp("digest");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let e = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap_err();
        assert!(e.to_string().contains("hashes to"), "{e}");
        assert!(!store.has(&Digest::parse(&layer).unwrap()));
        let _ = std::fs::remove_dir_all(&root);
    }
    /// The audit's A11: an image found in the store again is checked as its pull checked
    /// it. A config or manifest changed under its digest is refused from the store alone,
    /// as a changed copy of its own kind, and the next pull fetches it again in its place.
    #[test]
    fn a_stored_image_whose_documents_changed_is_refused_then_mended() {
        let fake = fake(image("arm64", &[("a", b"a")], true), None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let root = temp("changed");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let pulled = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap();
        let found = local(&store, &reference, &arm64(), &Limits::none())
            .unwrap()
            .unwrap();
        assert_eq!(
            (&found.manifest, &found.config, &found.rootfs),
            (&pulled.manifest, &pulled.config, &pulled.rootfs)
        );
        // Its index labelled it for arm64, and guests of another platform do not run it.
        let e = local(&store, &reference, &riscv64(), &Limits::none()).unwrap_err();
        assert!(
            e.to_string().contains("is for linux/arm64, not linux/riscv64"),
            "{e}"
        );

        let recorded = store.tagged(&reference.to_string()).unwrap().unwrap();
        let manifest = store.content(&recorded, oci::MAX_MANIFEST).unwrap().unwrap();
        let Document::Manifest(parsed) = oci::parse_document(&manifest, "").unwrap() else {
            panic!("an index");
        };
        let config_path = store.blob_path(&parsed.config.digest().unwrap());
        let manifest_path = store.blob_path(&recorded.digest().unwrap());
        let config = String::from_utf8(std::fs::read(&config_path).unwrap()).unwrap();
        let text = String::from_utf8(manifest.clone()).unwrap();
        let mut truncated = config.clone();
        truncated.pop();
        let cases = [
            (
                &config_path,
                config.replace("/bin/sh", "/bin/xx"),
                "a config's valid JSON changed",
            ),
            (&config_path, truncated, "a config truncated"),
            (&config_path, format!("{config} "), "a config grown"),
            (
                &manifest_path,
                text.replace("\"schemaVersion\":2", "\"schemaVersion\":3"),
                "a manifest changed",
            ),
        ];
        for (path, changed, why) in cases {
            let original = std::fs::read(path).unwrap();
            std::fs::write(path, changed).unwrap();
            let e = local(&store, &reference, &arm64(), &Limits::none()).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Changed, "{why}: {e}");
            assert!(
                e.to_string().contains("the stored copy has changed"),
                "{why}: {e}"
            );
            let mended = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap();
            assert_eq!(mended.config, pulled.config, "{why}");
            assert_eq!(std::fs::read(path).unwrap(), original, "{why}: not mended");
            let found = local(&store, &reference, &arm64(), &Limits::none())
                .unwrap()
                .unwrap();
            assert_eq!(found.config, pulled.config, "{why}");
        }
        assert!(
            local(&store, &reference, &arm64(), &Limits::none())
                .unwrap()
                .is_some()
        );
        // A layer changed where it is stored, its root filesystem gone: the next pull
        // fetches the layer again, where its DiffID would fail every build.
        let layer = parsed.layers[0].digest().unwrap();
        let layer_path = store.blob_path(&layer);
        let original = std::fs::read(&layer_path).unwrap();
        let mut changed = original.clone();
        let middle = changed.len() / 2;
        changed[middle] ^= 0xff;
        std::fs::write(&layer_path, &changed).unwrap();
        std::fs::remove_file(pulled.rootfs.as_ref().unwrap()).unwrap();
        let mended = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap();
        assert_eq!(mended.rootfs, pulled.rootfs);
        assert_eq!(std::fs::read(&layer_path).unwrap(), original, "the layer mended");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An index's label outranks the platform its manifest's config names, as containerd
    /// ranks them, when the image is found again too.
    #[test]
    fn an_index_label_decides_the_platform_of_a_stored_image_too() {
        let fake = fake(labelled("arm64", "amd64", &[("a", b"a")], true), None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let root = temp("labelled");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap();
        assert!(
            local(&store, &reference, &arm64(), &Limits::none())
                .unwrap()
                .is_some()
        );
        let e = local(&store, &reference, &riscv64(), &Limits::none()).unwrap_err();
        assert!(e.to_string().contains("is for linux/arm64, not"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A manifest that names no media type of its own is read again as the type the
    /// registry served it as.
    #[test]
    fn a_stored_manifest_is_read_as_the_type_it_was_served_as() {
        let layer = gzip(&tar("a", b"a"));
        let config = format!(
            r#"{{"architecture":"arm64","os":"linux","rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
            sha256(&tar("a", b"a"))
        )
        .into_bytes();
        let manifest = format!(
            r#"{{"schemaVersion":2,"config":{},"layers":[{}]}}"#,
            descriptor(CONFIGS[0], &config),
            descriptor(&format!("{}+gzip", oci::media::OCI_LAYER), &layer)
        )
        .into_bytes();
        let fake = fake(unindexed(manifest, &[&config, &layer]), None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let root = temp("untyped");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let pulled = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {}).unwrap();
        let found = local(&store, &reference, &arm64(), &Limits::none())
            .unwrap()
            .unwrap();
        assert_eq!(found.manifest, pulled.manifest);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An image made of `manifest` and `blobs`, tagged `v1` with no index above it.
    fn unindexed(manifest: Vec<u8>, blobs: &[&[u8]]) -> Image {
        let mut manifests = HashMap::new();
        for key in [sha256(&manifest), "v1".to_string()] {
            manifests.insert(key, (manifest.clone(), oci::media::OCI_MANIFEST.to_string()));
        }
        Image {
            manifests,
            blobs: blobs.iter().map(|b| (sha256(b), b.to_vec())).collect(),
            layers: Vec::new(),
        }
    }

    /// Stores `manifest` and `blobs` and records `reference` as naming the manifest, as a
    /// pull that did not check them would have.
    fn record(store: &Store, reference: &Reference, manifest: &[u8], blobs: &[&[u8]]) {
        for blob in blobs.iter().chain([&manifest]) {
            let digest = Digest::parse(&sha256(blob)).unwrap();
            store.ingest(&digest, blob.len() as u64, &mut &blob[..]).unwrap();
        }
        let desc = Descriptor {
            media_type: oci::media::OCI_MANIFEST.into(),
            digest: sha256(manifest),
            size: i64::try_from(manifest.len()).unwrap(),
            platform: None,
            annotations: Default::default(),
        };
        let digest = Digest::parse(&desc.digest).unwrap();
        store.tag(&reference.to_string(), &desc, &digest, &[]).unwrap();
    }

    /// A push asks for push access to its repository and pull access to the one it
    /// mounts from, as containerd's pusher scopes them; a blob there already is not sent,
    /// one another repository holds is mounted, and the manifest goes last, by its tag.
    #[test]
    fn pushes_ask_for_push_access_and_mount_what_they_can() {
        let dir = temp("push");
        let store = Store::open(&dir).unwrap();
        let (config, layer) = (
            br#"{"architecture":"arm64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec(),
            b"layer".to_vec(),
        );
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{},"layers":[{}]}}"#,
            oci::media::OCI_MANIFEST,
            descriptor("application/vnd.oci.image.config.v1+json", &config),
            descriptor("application/vnd.oci.image.layer.v1.tar", &layer)
        )
        .into_bytes();
        let reference = Reference::parse("127.0.0.1:1/test/image:v1").unwrap();
        record(&store, &reference, &manifest, &[&config, &layer]);
        let tokens: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let asked = tokens.clone();
        let port = Arc::new(AtomicU16::new(0));
        let own = port.clone();
        let (config_digest, layer_digest) = (sha256(&config), sha256(&layer));
        let server = route(None, move |req: &Seen| {
            let path = req.target.split('?').next().unwrap_or_default();
            if path == "/token" {
                asked.lock().unwrap().push(req.target.clone());
                let token = format!(r#"{{"token":"{TOKEN}","expires_in":300}}"#);
                return Some((http("200 OK", &[], token.as_bytes()), After::Keep));
            }
            if req.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
                let challenge = format!(
                    r#"Bearer realm="http://127.0.0.1:{}/token",service="fake",scope="repository:test/image:pull""#,
                    own.load(Ordering::SeqCst)
                );
                return Some((
                    http("401 Unauthorized", &[("WWW-Authenticate", challenge)], b""),
                    After::Keep,
                ));
            }
            let status = match (req.method.as_str(), path) {
                ("HEAD", p) if p == format!("/v2/test/image/blobs/{layer_digest}") => "200 OK",
                ("HEAD", _) => "404 Not Found",
                ("POST", "/v2/test/image/blobs/uploads/")
                    if req.target.contains(&format!("mount={config_digest}")) =>
                {
                    "201 Created"
                }
                ("PUT", "/v2/test/image/manifests/v1") => "201 Created",
                _ => "400 Bad Request",
            };
            Some((http(status, &[], b""), After::Keep))
        });
        port.store(server.port, Ordering::SeqCst);
        let at = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let registry =
            Registry::for_push(client(), &at, Credentials::default(), &["team/src".into()]).unwrap();
        let desc = store.tagged(&reference.to_string()).unwrap().unwrap();
        let fates = std::sync::Mutex::new(Vec::new());
        crate::push::push(&registry, &store, &desc, Some("v1"), Some("team/src"), &|d, f| {
            fates.lock().unwrap().push((d.to_string(), f));
        })
        .unwrap();
        assert_eq!(
            fates.into_inner().unwrap(),
            [(sha256(&layer), crate::push::Layer::Exists)]
        );
        let decoded: Vec<String> = tokens
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.replace("%3A", ":").replace("%2F", "/").replace("%2C", ","))
            .collect();
        assert!(
            decoded
                .iter()
                .any(|t| t.contains("repository:test/image:pull,push")),
            "{decoded:?}"
        );
        assert!(
            decoded.iter().any(|t| t.contains("repository:team/src:pull")),
            "{decoded:?}"
        );
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r.starts_with("PUT /v2/test/image/manifests/v1")),
            "{:?}",
            server.requests()
        );
    }

    /// The audit's A11: what a pull refuses, a stored image is refused for too, with the
    /// same words: layer and DiffID counts that differ, DiffIDs that are not digests, a
    /// config of the wrong type, platform or rootfs type, or past the limit, and layers
    /// of no layer type. A config whose descriptor is wrong about its size cannot be
    /// downloaded, and is refused when stored.
    #[test]
    fn stored_images_are_refused_what_a_pull_refuses() {
        let layer = gzip(&tar("a", b"a"));
        let diff_id = sha256(&tar("a", b"a"));
        let gzip_layer = format!("{}+gzip", oci::media::OCI_LAYER);
        let config = |arch: &str, kind: &str, ids: &[&str]| {
            let ids: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
            format!(
                r#"{{"architecture":"{arch}","os":"linux","rootfs":{{"type":"{kind}","diff_ids":[{}]}}}}"#,
                ids.join(",")
            )
            .into_bytes()
        };
        let manifest = |config: &str, layers: &[String]| {
            format!(
                r#"{{"schemaVersion":2,"mediaType":"{}","config":{config},"layers":[{}]}}"#,
                oci::media::OCI_MANIFEST,
                layers.join(",")
            )
            .into_bytes()
        };
        let one = [descriptor(&gzip_layer, &layer)];
        let two = [descriptor(&gzip_layer, &layer), descriptor(&gzip_layer, &layer)];
        let right = config("arm64", "layers", &[&diff_id]);
        let over = descriptor(CONFIGS[0], &right).replace(
            &format!("\"size\":{}", right.len()),
            &format!("\"size\":{}", oci::MAX_CONFIG + 1),
        );
        let wrong_size = descriptor(CONFIGS[0], &right).replace(
            &format!("\"size\":{}", right.len()),
            &format!("\"size\":{}", right.len() + 1),
        );
        /// A stored image with one defect, and what a pull and a lookup of it say.
        struct Case {
            why: &'static str,
            config: Vec<u8>,
            manifest: Vec<u8>,
            pulled: &'static str,
            found: &'static str,
        }
        let cases = [
            Case {
                why: "two layers, one DiffID",
                config: right.clone(),
                manifest: manifest(&descriptor(CONFIGS[0], &right), &two),
                pulled: "2 layers but 1 DiffIDs",
                found: "2 layers but 1 DiffIDs",
            },
            Case {
                why: "one layer, two DiffIDs",
                config: config("arm64", "layers", &[&diff_id, &diff_id]),
                manifest: manifest(
                    &descriptor(CONFIGS[0], &config("arm64", "layers", &[&diff_id, &diff_id])),
                    &one,
                ),
                pulled: "1 layers but 2 DiffIDs",
                found: "1 layers but 2 DiffIDs",
            },
            Case {
                why: "a DiffID that is not a digest",
                config: config("arm64", "layers", &["sha256:abc"]),
                manifest: manifest(
                    &descriptor(CONFIGS[0], &config("arm64", "layers", &["sha256:abc"])),
                    &one,
                ),
                pulled: "invalid checksum digest",
                found: "invalid checksum digest",
            },
            Case {
                why: "a rootfs that is not layers",
                config: config("arm64", "other", &[&diff_id]),
                manifest: manifest(
                    &descriptor(CONFIGS[0], &config("arm64", "other", &[&diff_id])),
                    &one,
                ),
                pulled: "is not \"layers\"",
                found: "is not \"layers\"",
            },
            Case {
                why: "an image for another platform",
                config: config("s390x", "layers", &[&diff_id]),
                manifest: manifest(
                    &descriptor(CONFIGS[0], &config("s390x", "layers", &[&diff_id])),
                    &one,
                ),
                pulled: "is for linux/s390x, not linux/arm64",
                found: "is for linux/s390x, not linux/arm64",
            },
            Case {
                why: "a config that is not an image's",
                config: right.clone(),
                manifest: manifest(&descriptor("application/vnd.example+json", &right), &one),
                pulled: "not a container image",
                found: "not a container image",
            },
            Case {
                why: "a config past the limit",
                config: right.clone(),
                manifest: manifest(&over, &one),
                pulled: "config is over the 4194304-byte limit",
                found: "config is over the 4194304-byte limit",
            },
            Case {
                why: "a layer of no layer type",
                config: right.clone(),
                manifest: manifest(
                    &descriptor(CONFIGS[0], &right),
                    &[descriptor("application/octet-stream", &layer)],
                ),
                pulled: "not a layer",
                found: "not a layer",
            },
            Case {
                why: "a config whose descriptor is wrong about its size",
                config: right.clone(),
                manifest: manifest(&wrong_size, &one),
                pulled: "without progress",
                found: "where its descriptor says",
            },
        ];
        for Case {
            why,
            config,
            manifest,
            pulled,
            found,
        } in cases
        {
            let fake = fake(unindexed(manifest.clone(), &[&config, &layer]), None);
            let reference =
                Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
            let root = temp("refused");
            let store = Store::open(&root).unwrap();
            let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
            let e = pull(&registry, &store, &reference, &arm64(), &Limits::none(), &|_| {})
                .unwrap_err()
                .to_string();
            assert!(e.contains(pulled), "{why}, pulled: {e}");
            assert_eq!(store.tagged(&reference.to_string()).unwrap(), None, "{why}");

            record(&store, &reference, &manifest, &[&config, &layer]);
            let e = local(&store, &reference, &arm64(), &Limits::none())
                .unwrap_err()
                .to_string();
            assert!(e.contains(found), "{why}, found: {e}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// An image whose layers are larger than it may decompress to is refused before any
    /// of them is downloaded (audit A10).
    #[test]
    fn an_image_past_its_bytes_downloads_nothing() {
        // Bytes gzip cannot shrink: a xorshift's.
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let big: Vec<u8> = (0..300_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let fake = fake(image("arm64", &[("big", &big)], true), None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let root = temp("past-bytes");
        let store = Store::open(&root).unwrap();
        let registry = Registry::new(client(), &reference, Credentials::Anonymous).unwrap();
        let limits = Limits {
            bytes: 100_000,
            ..Limits::none()
        };
        let e = pull(&registry, &store, &reference, &arm64(), &limits, &|_| {}).unwrap_err();
        assert!(e.to_string().contains("(SHARDS_MAX_IMAGE_BYTES)"), "{e}");
        assert!(fake.cdn.requests().is_empty(), "{:?}", fake.cdn.requests());
        let _ = std::fs::remove_dir_all(&root);
    }
}
