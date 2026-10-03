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

/// Blobs uploaded at once: dockerd's default `max-concurrent-uploads` (daemon/config
/// config.go, DefaultMaxConcurrentUploads).
const CONCURRENT: usize = 5;

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
    Error::of(ErrorKind::Missing, format!("content digest {d}: not found"))
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
            match store.held(child, oci::MAX_MANIFEST)? {
                Held::Whole(_) => {}
                Held::Invalid(why) => return Err(Error::new(why)),
                Held::Missing | Held::Changed(_) => return Err(missing(&child.digest)),
            }
        }
        for child in &index.manifests {
            push_manifest(registry, store, child, None, from, report)?;
        }
        if registry.exists(target, true, tag)? {
            return Ok(());
        }
        return put(registry, target, tag, &bytes);
    }
    push_manifest(registry, store, target, tag, from, report)
}

/// The bytes of a stored manifest or index, whole.
fn document(store: &Store, desc: &Descriptor) -> Result<Vec<u8>, Error> {
    match store.held(desc, oci::MAX_MANIFEST)? {
        Held::Whole(b) => Ok(b),
        Held::Invalid(why) => Err(Error::new(why)),
        Held::Missing | Held::Changed(_) => Err(missing(&desc.digest)),
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
    // The config and the layers together, as containerd dispatches a manifest's children
    // under one limiter; the layers' fates are what is told.
    let blobs: Vec<&Descriptor> = std::iter::once(&manifest.config)
        .chain(&manifest.layers)
        .collect();
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<Error>> = Mutex::new(None);
    let stopped = || failed.lock().unwrap_or_else(PoisonError::into_inner).is_some();
    let work = || {
        while !stopped() {
            let n = next.fetch_add(1, Ordering::Relaxed);
            let Some(desc) = blobs.get(n) else {
                return;
            };
            match blob(registry, store, desc, from).and_then(|fate| Ok((desc.digest()?, fate))) {
                Ok((digest, fate)) if n > 0 => report(&digest, fate),
                Ok(_) => {}
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
        let workers: Vec<_> = (0..CONCURRENT.min(blobs.len()))
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
    if registry.exists(desc, true, tag)? {
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
    if registry.exists(desc, false, None)? {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::auth::Credentials;
    use crate::http::Client;
    use crate::testing::{After, route};
    use sha2::{Digest as _, Sha256};
    use shards_image::reference::{Algorithm, Reference};

    fn described(media_type: &str, bytes: &[u8]) -> Descriptor {
        Descriptor {
            media_type: media_type.into(),
            digest: Digest::from_hash(Algorithm::Sha256, &Sha256::digest(bytes)).to_string(),
            size: i64::try_from(bytes.len()).unwrap(),
            platform: None,
            annotations: Default::default(),
        }
    }

    /// What is not here to push is `Missing`, and what the registry has no room for
    /// `NotFound`: a caller that pushes less where content is missing here does not take
    /// a registry's 404 for that.
    #[test]
    fn missing_content_and_a_registrys_404_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("shards-push-kinds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        let server = route(None, |_| {
            Some((
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                After::Keep,
            ))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = Client::new(
            Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
            "shards-test",
        );
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let config = br#"{"architecture":"arm64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
        let config_type = "application/vnd.oci.image.config.v1+json";
        let config_desc = described(config_type, config);
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"{}","digest":"{}","size":{}}},"layers":[]}}"#,
            oci::media::OCI_MANIFEST,
            config_type,
            config_desc.digest,
            config.len()
        )
        .into_bytes();
        let manifest_desc = described(oci::media::OCI_MANIFEST, &manifest);
        let quiet = |_: &Digest, _: Layer| {};
        // Nothing of it here.
        let missing = push(&registry, &store, &manifest_desc, Some("v1"), None, &quiet).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::Missing, "{missing}");
        // All of it here, and the registry answering 404.
        for (desc, bytes) in [(&config_desc, &config[..]), (&manifest_desc, &manifest[..])] {
            store
                .ingest(&desc.digest().unwrap(), bytes.len() as u64, &mut &bytes[..])
                .unwrap();
        }
        let refused = push(&registry, &store, &manifest_desc, Some("v1"), None, &quiet).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::NotFound, "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
