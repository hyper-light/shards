//! A pull, as `docker pull` makes one (docs/research/registry-pull.md R1, R6):
//! 1. resolve the reference;
//! 2. choose the manifest for our guests' platform;
//! 3. fetch the config and check its layers;
//! 4. fetch the layers, three at a time;
//! 5. build the image's root filesystem and record the reference.
//!
//! Nothing counts as pulled until every size and digest, and every layer's DiffID, has
//! been checked. An image found in the store again is checked as its pull checked it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use shards_image::oci::{self, Descriptor, Document, ImageConfig, Manifest, Platform};
use shards_image::platform::{self, Target};
use shards_image::reference::{Digest, Reference};
use shards_image::store::{Held, Layer, Limits, Store};

use crate::registry::Registry;
use crate::{Error, ErrorKind};

/// Layers fetched at once: dockerd's default `max-concurrent-downloads`.
const CONCURRENT: usize = 3;
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
    pub config: ImageConfig,
    /// The image's EROFS root filesystem.
    pub rootfs: PathBuf,
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
}

/// Pulls `reference` for the platforms `targets` into `store`, its root filesystem built
/// within `limits`.
pub fn pull(
    registry: &Registry,
    store: &Store,
    reference: &Reference,
    targets: &[Target],
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
) -> Result<Pulled, Error> {
    let name = reference.familiar();
    // What it writes is recorded only at its end: no collection runs meanwhile.
    let _lease = store.lease()?;
    let top = registry.resolve(store, reference)?;
    let resolved = top.digest()?;
    let (manifest_desc, manifest) = match document(registry, store, &top)? {
        Document::Manifest(m) => (top, m),
        Document::Index(index) => {
            let chosen = platform::select(&index, targets).ok_or_else(|| {
                let offered: Vec<String> = index
                    .manifests
                    .iter()
                    .filter_map(|d| d.platform.as_ref())
                    .map(|p| format!("{}/{}", p.os, p.architecture))
                    .collect();
                Error::new(format!(
                    "{name}: no manifest for {} among [{}]",
                    wanted(targets),
                    offered.join(", ")
                ))
            })?;
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

    registry.fetch_blob(store, &manifest.config, limits, &|_| {})?;
    // A stored config that has changed is fetched again in its place.
    let config = match stored(store, &name, &manifest.config, oci::MAX_CONFIG) {
        Err(e) if e.kind() == ErrorKind::Changed => {
            registry.fetch_blob_again(store, &manifest.config, limits, &|_| {})?;
            stored(store, &name, &manifest.config, oci::MAX_CONFIG)?
        }
        read => read?,
    };
    let (config, layers) = checked(&name, &manifest_desc, &manifest, &config, targets)?;

    fetch_layers(registry, store, &manifest, limits, report)?;
    report(Event::Building);
    let rootfs = store.rootfs(&layers, limits)?;
    let mut contents = vec![manifest_digest.clone(), manifest.config.digest()?];
    contents.extend(layers.iter().map(|l| l.blob.clone()));
    store.tag(&reference.to_string(), &manifest_desc, &resolved, &contents)?;
    Ok(Pulled {
        resolved,
        manifest: manifest_digest,
        config,
        rootfs,
    })
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
    let name = reference.familiar();
    let manifest_digest = manifest_desc.digest()?;
    let bytes = stored(store, &name, &manifest_desc, oci::MAX_MANIFEST)?;
    let Document::Manifest(manifest) = oci::parse_document(&bytes, &manifest_desc.media_type)? else {
        return Err(Error::new(format!("{name}: its record names an index")));
    };
    contents(&name, &manifest)?;
    let config = stored(store, &name, &manifest.config, oci::MAX_CONFIG)?;
    let (config, layers) = checked(&name, &manifest_desc, &manifest, &config, targets)?;
    let rootfs = store.rootfs(&layers, limits)?;
    let resolved = store
        .resolved(&reference.to_string())?
        .unwrap_or_else(|| manifest_digest.clone());
    Ok(Some(Pulled {
        resolved,
        manifest: manifest_digest,
        config,
        rootfs,
    }))
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
        Held::Missing => Err(Error::new(format!("{name}: {} is not in the store", desc.digest))),
    }
}

/// An index or manifest, fetched and parsed by its descriptor's media type.
fn document(registry: &Registry, store: &Store, desc: &Descriptor) -> Result<Document, Error> {
    let bytes = registry.fetch_document(store, desc)?;
    Ok(oci::parse_document(&bytes, &desc.media_type)?)
}

/// Fetches the layers, `CONCURRENT` at a time. The first failure stops the rest.
fn fetch_layers(
    registry: &Registry,
    store: &Store,
    manifest: &Manifest,
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
) -> Result<(), Error> {
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<Error>> = Mutex::new(None);
    let stopped = || failed.lock().unwrap_or_else(PoisonError::into_inner).is_some();
    let work = || {
        while !stopped() {
            let Some(layer) = manifest.layers.get(next.fetch_add(1, Ordering::Relaxed)) else {
                return;
            };
            let fetched = layer.digest().map_err(Error::from).and_then(|digest| {
                if store.has(&digest) {
                    report(Event::Present(&digest));
                    return Ok(());
                }
                registry.fetch_blob(store, layer, limits, &|n| report(Event::Progress(&digest, n)))?;
                report(Event::Layer(&digest));
                Ok(())
            });
            if let Err(e) = fetched {
                failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_or_insert(e);
            }
        }
    };
    std::thread::scope(|scope| {
        let mut started = 0;
        for _ in 0..CONCURRENT.min(manifest.layers.len()) {
            if std::thread::Builder::new()
                .name("shards-pull".into())
                .spawn_scoped(scope, work)
                .is_ok()
            {
                started += 1;
            }
        }
        // No thread to spare: fetch them here.
        if started == 0 {
            work();
        }
    });
    match failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        Some(e) => Err(e),
        None => Ok(()),
    }
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
    use std::sync::atomic::{AtomicBool, AtomicU16};

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
            if cut.as_deref() == Some(digest) && !cut_done.swap(true, Ordering::SeqCst) {
                let mut head = http("200 OK", &[], bytes);
                head.truncate(head.len() - bytes.len() / 2);
                return Some((head, After::Close));
            }
            let from: usize = req
                .header("range")
                .and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse().ok())
                .unwrap_or(0);
            if from > 0 {
                let range = format!("bytes {from}-{}/{}", bytes.len() - 1, bytes.len());
                let partial = http("206 Partial Content", &[("Content-Range", range)], &bytes[from..]);
                return Some((partial, After::Keep));
            }
            Some((http("200 OK", &[], bytes), After::Keep))
        });
        let cdn_port = cdn.port;
        let port = Arc::new(AtomicU16::new(0));
        let own = port.clone();
        let registry = route(None, move |req: &Seen| {
            let path = req.target.split('?').next().unwrap_or_default();
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
            Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
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
            if let Event::Layer(d) = e {
                events.lock().unwrap().push(d.to_string());
            }
        })
        .unwrap();
        let image_bytes = std::fs::read(&pulled.rootfs).unwrap();
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
        assert_eq!(events.lock().unwrap().len(), 2);
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
        let fake = fake(image("arm64", &[("a", b"a")], true), None);
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
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
        assert!(
            e.to_string()
                .contains("no manifest for linux/riscv64 among [linux/s390x, linux/arm64]"),
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
        let shown = e.to_string();
        assert!(shown.contains("limit 100 per 21600 s; 0 left"), "{shown}");
        assert!(
            shown.contains("counted for 192.0.2.1; retry after 3600"),
            "{shown}"
        );
        assert_eq!(server.requests().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
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
