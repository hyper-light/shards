//! A pull, as `docker pull` makes one (docs/research/registry-pull.md R1, R6):
//! 1. resolve the reference;
//! 2. choose the manifest for our guests' platform;
//! 3. fetch the config and check its layers;
//! 4. fetch the layers, three at a time;
//! 5. build the image's root filesystem and record the reference.
//!
//! Nothing counts as pulled until every size and digest, and every layer's DiffID, has
//! been checked.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use shards_image::oci::{self, Descriptor, Document, ImageConfig, Manifest, Platform};
use shards_image::platform::{self, Target};
use shards_image::reference::{Digest, Reference};
use shards_image::store::{Layer, Store};

use crate::Error;
use crate::registry::Registry;

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
    /// Bytes of a layer arrived.
    Progress(&'a Digest, u64),
    /// A layer is stored and verified.
    Layer(&'a Digest),
    /// The root filesystem is being built.
    Building,
}

/// Pulls `reference` for the platforms `targets` into `store`. A layer may unpack to at
/// most `max_layer` bytes.
pub fn pull(
    registry: &Registry,
    store: &Store,
    reference: &Reference,
    targets: &[Target],
    max_layer: u64,
    report: &(dyn Fn(Event<'_>) + Sync),
) -> Result<Pulled, Error> {
    let name = reference.familiar();
    let top = registry.resolve(store, reference)?;
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
    if !CONFIGS.contains(&manifest.config.media_type.as_str()) {
        return Err(Error::new(format!(
            "{name}: not a container image: its config is {:?}",
            manifest.config.media_type
        )));
    }
    for layer in &manifest.layers {
        oci::layer_compression(&layer.media_type)?;
    }
    report(Event::Manifest(&manifest_digest, &manifest.layers));

    registry.fetch_blob(store, &manifest.config, &|_| {})?;
    let config_digest = manifest.config.digest()?;
    let config = oci::read_config(File::open(store.blob_path(&config_digest))?)?;
    // An unlabelled manifest is checked by its config, as containerd checks it.
    if manifest_desc.platform.is_none() {
        let own = platform::normalize(&Platform {
            architecture: config.architecture.clone(),
            os: config.os.clone(),
            variant: config.variant.clone(),
            os_features: Vec::new(),
        });
        if !targets.contains(&own) {
            return Err(Error::new(format!(
                "{name} is for {}/{}, not {}",
                config.os,
                config.architecture,
                wanted(targets)
            )));
        }
    }
    // image-spec config.md: one DiffID per layer.
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

    fetch_layers(registry, store, &manifest, report)?;
    report(Event::Building);
    let rootfs = store.rootfs(&layers, max_layer)?;
    store.tag(&reference.to_string(), &manifest_digest)?;
    Ok(Pulled {
        manifest: manifest_digest,
        config,
        rootfs,
    })
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
                registry.fetch_blob(store, layer, &|n| report(Event::Progress(&digest, n)))?;
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
            r#"{{"architecture":"{arch}","os":"linux","config":{{"Cmd":["/bin/sh"]}},"rootfs":{{"type":"layers","diff_ids":[{}]}}}}"#,
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

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shards-pull-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        let pulled = pull(&registry, &store, &reference, &arm64(), 1 << 30, &|e| {
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
        assert_eq!(
            store.tagged(&reference.to_string()).unwrap(),
            Some(pulled.manifest.clone())
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
        pull(&registry, &store, &reference, &arm64(), 1 << 30, &|_| {}).unwrap();
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
        let riscv = [Target {
            os: "linux".into(),
            architecture: "riscv64".into(),
            variant: String::new(),
        }];
        let e = pull(&registry, &store, &reference, &riscv, 1 << 30, &|_| {}).unwrap_err();
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
        let e = pull(&registry, &store, &reference, &arm64(), 1 << 30, &|_| {}).unwrap_err();
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
        let e = pull(&registry, &store, &reference, &arm64(), 1 << 30, &|_| {}).unwrap_err();
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
        let e = pull(&registry, &store, &reference, &arm64(), 1 << 30, &|_| {}).unwrap_err();
        assert!(e.to_string().contains("hashes to"), "{e}");
        assert!(!store.has(&Digest::parse(&layer).unwrap()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
