//! Pushing images to OCI registries, as containerd's PushContent walks an image and its
//! docker pusher sends it (containerd v2 core/remotes/handlers.go, PushContent;
//! core/remotes/docker/pusher.go): what each manifest names before the manifest, every
//! child manifest of an index before the index, each blob only if the repository lacks
//! it, mounted from another repository of the registry where one holds it, else uploaded
//! with a POST and one PUT. The target goes last, by its tag.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use shards_image::oci::{self, Descriptor, Document};
use shards_image::reference::Digest;
use shards_image::store::{Held, Store};

use crate::registry::Registry;
use crate::{Error, ErrorKind};

/// Layers uploaded at once, as many as a pull fetches (pull.rs).
const CONCURRENT: usize = 3;

/// What happened to a layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    Pushed,
    Exists,
    Mounted(String),
}

/// A blob or document the store lacks: containerd's words, and its kind, so that the
/// caller can push less.
fn missing(d: &str) -> Error {
    Error::of(ErrorKind::NotFound, format!("content digest {d}: not found"))
}

/// Pushes what `target` describes, from `store`, to `registry`, `target` itself by
/// `tag` if given, else by its digest. Blobs are mounted from `from`, a repository of the
/// same registry, when given. `report` hears each layer's fate.
pub fn push(
    registry: &Registry,
    store: &Store,
    target: &Descriptor,
    tag: Option<&str>,
    from: Option<&str>,
    report: &(dyn Fn(&Digest, Layer) + Sync),
) -> Result<(), Error> {
    let bytes = document(store, target)?;
    if let Document::Index(index) = oci::parse_document(&bytes, &target.media_type)? {
        // Every child first: an index whose content is not all here is not pushed.
        for child in &index.manifests {
            if !matches!(store.held(child, oci::MAX_MANIFEST)?, Held::Whole(_)) {
                return Err(missing(&child.digest));
            }
        }
        for child in &index.manifests {
            push_manifest(registry, store, child, None, from, report)?;
        }
        return put(registry, target, tag, &bytes);
    }
    push_manifest(registry, store, target, tag, from, report)
}

/// The bytes of a stored manifest or index, whole.
fn document(store: &Store, desc: &Descriptor) -> Result<Vec<u8>, Error> {
    match store.held(desc, oci::MAX_MANIFEST)? {
        Held::Whole(b) => Ok(b),
        _ => Err(missing(&desc.digest)),
    }
}

/// A manifest: its config and layers, then it, by `tag` or its digest, unless the
/// repository has it already.
fn push_manifest(
    registry: &Registry,
    store: &Store,
    desc: &Descriptor,
    tag: Option<&str>,
    from: Option<&str>,
    report: &(dyn Fn(&Digest, Layer) + Sync),
) -> Result<(), Error> {
    let bytes = document(store, desc)?;
    let Document::Manifest(manifest) = oci::parse_document(&bytes, &desc.media_type)? else {
        return Err(Error::new(format!("{}: an index inside an index", desc.digest)));
    };
    for blob in std::iter::once(&manifest.config).chain(&manifest.layers) {
        if !store.has(&blob.digest()?) {
            return Err(missing(&blob.digest));
        }
    }
    blob(registry, store, &manifest.config, from)?;
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<Error>> = Mutex::new(None);
    let stopped = || failed.lock().unwrap_or_else(PoisonError::into_inner).is_some();
    let work = || {
        while !stopped() {
            let Some(layer) = manifest.layers.get(next.fetch_add(1, Ordering::Relaxed)) else {
                return;
            };
            match blob(registry, store, layer, from).and_then(|fate| Ok((layer.digest()?, fate))) {
                Ok((digest, fate)) => report(&digest, fate),
                Err(e) => {
                    failed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get_or_insert(e);
                }
            }
        }
    };
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..CONCURRENT.min(manifest.layers.len()))
            .filter_map(|_| std::thread::Builder::new().spawn_scoped(scope, work).ok())
            .collect();
        // Without a thread to spare, this one does the work.
        if workers.is_empty() {
            work();
        }
    });
    if let Some(e) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(e);
    }
    if tag.is_none() && registry.has_manifest(desc)? {
        return Ok(());
    }
    put(registry, desc, tag, &bytes)
}

/// A manifest or index put by `tag`, or by its digest.
fn put(registry: &Registry, desc: &Descriptor, tag: Option<&str>, bytes: &[u8]) -> Result<(), Error> {
    registry.put_manifest(tag.unwrap_or(&desc.digest), &desc.media_type, bytes)
}

/// One blob: there already, mounted, or uploaded.
fn blob(registry: &Registry, store: &Store, desc: &Descriptor, from: Option<&str>) -> Result<Layer, Error> {
    if registry.has_blob(desc)? {
        return Ok(Layer::Exists);
    }
    let digest = desc.digest()?;
    let file =
        std::fs::File::open(store.blob_path(&digest)).map_err(|e| Error::new(format!("{digest}: {e}")))?;
    Ok(match registry.upload(desc, &file, from)? {
        true => Layer::Mounted(from.unwrap_or_default().to_string()),
        false => Layer::Pushed,
    })
}
