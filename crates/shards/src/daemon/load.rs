//! `shards load` as dockerd's containerd store loads an archive (moby docker-v29.3.1
//! daemon/containerd/image_exporter.go, LoadImage; containerd v2 core/images/archive
//! importer.go, ImportIndex; client/import.go, Import): every regular file of the tar
//! ingested as a blob, hashed as it streams; then the images an OCI layout's
//! `index.json` names, or those Docker's `manifest.json` describes, made into manifests
//! as containerd makes them; each recorded by its name, or kept by its digest
//! (`moby-dangling@`) if it has none; each unpacked for our platform, if it has one.
//!
//! Unlike dockerd: a name that is not a reference is not recorded as it is written (the
//! store records references); the image is kept by its digest instead. An archive
//! compressed with bzip2 or xz is refused; gzip and zstd are read, as is a plain tar.
//! Zstd layers in Docker's manifest.json get zstd's media type, where containerd gives
//! every compressed layer gzip's, which no unpacker could then read.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};

use shards_image::oci::{self, Descriptor};
use shards_image::reference::{Digest, Reference};
use shards_image::store::Store;

/// The largest `oci-layout` or `manifest.json` read: containerd reads them whole.
const MAX_JSON: u64 = 16 << 20;

const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar";
const DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

/// One of Docker's manifest.json's entries.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerManifest {
    config: String,
    #[serde(default)]
    repo_tags: Option<Vec<String>>,
    #[serde(default)]
    layers: Vec<String>,
}

/// What an archive's compression is, by its first bytes (containerd's
/// DetectCompression).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Zstd,
    Bzip2,
    Xz,
}

fn detect(head: &[u8]) -> Compression {
    if head.starts_with(&[0x1f, 0x8b, 0x08]) {
        Compression::Gzip
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Compression::Zstd
    } else if head.starts_with(b"BZh") {
        Compression::Bzip2
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Compression::Xz
    } else {
        Compression::None
    }
}

/// An image the archive names, as containerd's Import makes it: a name and what it
/// resolved to.
struct Named {
    name: String,
    target: Descriptor,
    dangling: bool,
}

/// Go's tar reader's words for an archive cut short.
fn go_tar(e: &shards_image::Error) -> String {
    let said = e.to_string();
    if said.contains("archive ends inside") {
        "unexpected EOF".into()
    } else {
        said
    }
}

/// ImportIndex: every regular file ingested, then the index the archive is: its
/// `index.json` in an OCI layout, or one made of Docker's `manifest.json`.
fn import(store: &Store, input: &mut dyn Read) -> Result<(oci::Index, Vec<Digest>), String> {
    let mut tar = shards_image::tar::Reader::new(input);
    let mut layout: Option<String> = None;
    let mut docker: Option<Vec<DockerManifest>> = None;
    let mut blobs: BTreeMap<String, (Digest, u64)> = BTreeMap::new();
    let mut symlinks: Vec<(String, String)> = Vec::new();
    while let Some(entry) = tar.next_entry().map_err(|e| go_tar(&e))? {
        let name = String::from_utf8_lossy(&entry.path).into_owned();
        match entry.kind {
            shards_image::tar::Type::Symlink => {
                let dir = name.rsplit_once('/').map_or("", |(d, _)| d);
                let link = String::from_utf8_lossy(&entry.link);
                let joined = if dir.is_empty() {
                    link.to_string()
                } else {
                    format!("{dir}/{link}")
                };
                symlinks.push((name, joined));
                continue;
            }
            shards_image::tar::Type::File => {}
            _ => continue,
        }
        match name.as_str() {
            "oci-layout" | "manifest.json" => {
                if entry.size > MAX_JSON {
                    return Err(format!("untar {name} {:?}: past {MAX_JSON} bytes", name));
                }
                let mut bytes = Vec::with_capacity(usize::try_from(entry.size).unwrap_or(0));
                tar.copy_data(&mut bytes).map_err(|e| go_tar(&e))?;
                if name == "oci-layout" {
                    #[derive(serde::Deserialize)]
                    struct Layout {
                        #[serde(rename = "imageLayoutVersion", default)]
                        version: String,
                    }
                    let l: Layout = serde_json::from_slice(&bytes)
                        .map_err(|e| format!("untar oci layout \"oci-layout\": {e}"))?;
                    layout = Some(l.version);
                } else {
                    docker = Some(
                        serde_json::from_slice(&bytes)
                            .map_err(|e| format!("untar manifest \"manifest.json\": {e}"))?,
                    );
                }
            }
            _ => {
                let mut w = store.writer().map_err(|e| e.to_string())?;
                tar.copy_data(&mut w).map_err(|e| go_tar(&e))?;
                let (digest, size) = w
                    .commit()
                    .map_err(|e| format!("failed to ingest {name:?}: {e}"))?;
                blobs.insert(name, (digest, size));
            }
        }
    }
    let contents: Vec<Digest> = blobs.values().map(|(d, _)| d.clone()).collect();
    if let Some(version) = layout.filter(|v| !v.is_empty()) {
        if version != "1.0.0" {
            return Err(format!("unsupported OCI version {version}"));
        }
        let (digest, size) = blobs
            .get("index.json")
            .ok_or("missing index.json in OCI layout 1.0.0")?;
        let desc = blob_desc(oci::media::OCI_INDEX, digest, *size);
        let bytes = match store.held(&desc, oci::MAX_MANIFEST).map_err(|e| e.to_string())? {
            shards_image::store::Held::Whole(b) => b,
            _ => return Err("index.json: not whole".into()),
        };
        let index: oci::Index = serde_json::from_slice(&bytes).map_err(|e| format!("index.json: {e}"))?;
        return Ok((index, contents));
    }
    let Some(manifests) = docker else {
        return Err("unrecognized image format".into());
    };
    for (name, target) in symlinks {
        let blob = blobs
            .get(&target)
            .cloned()
            .ok_or_else(|| format!("no target for symlink layer from {name:?} to {target:?}"))?;
        blobs.insert(name, blob);
    }
    let mut index = oci::Index {
        schema_version: 2,
        media_type: None,
        manifests: Vec::new(),
    };
    let mut written = contents;
    // containerd's resolveLayers: a layer whose content is already here compressed (a
    // stored image's layer of that DiffID) is that compressed blob.
    let compressed = compressed_layers(store);
    for m in manifests {
        let (digest, size) = blobs
            .get(&m.config)
            .ok_or_else(|| format!("image config {:?} not found", m.config))?;
        let config = blob_desc(DOCKER_CONFIG, digest, *size);
        let mut layers = Vec::with_capacity(m.layers.len());
        for l in &m.layers {
            let (digest, size) = blobs
                .get(l)
                .ok_or_else(|| format!("failed to resolve layers: layer {l:?} not found"))?;
            let (digest, size) = compressed.get(digest).unwrap_or(&(digest.clone(), *size)).clone();
            let (digest, size) = (&digest, &size);
            let mut head = [0u8; 10];
            let n = std::fs::File::open(store.blob_path(digest))
                .and_then(|mut f| f.read(&mut head))
                .map_err(|e| format!("failed to resolve layers: {e}"))?;
            let kind = match detect(head.get(..n).unwrap_or_default()) {
                Compression::None => DOCKER_LAYER,
                Compression::Zstd => OCI_LAYER_ZSTD,
                _ => DOCKER_LAYER_GZIP,
            };
            layers.push(blob_desc(kind, digest, *size));
        }
        let json = |d: &Descriptor| {
            format!(
                r#"{{"mediaType":{},"digest":{},"size":{}}}"#,
                serde_json::to_string(&d.media_type).unwrap_or_default(),
                serde_json::to_string(&d.digest).unwrap_or_default(),
                d.size
            )
        };
        let layers_json: Vec<String> = layers.iter().map(json).collect();
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{DOCKER_MANIFEST}","config":{},"layers":[{}]}}"#,
            json(&config),
            layers_json.join(",")
        );
        let mut w = store.writer().map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut w, manifest.as_bytes())
            .map_err(|e| format!("write docker manifest: {e}"))?;
        let (digest, size) = w.commit().map_err(|e| format!("write docker manifest: {e}"))?;
        written.push(digest.clone());
        // Its platform is containerd's to filter by; dockerd's record keeps none.
        let desc = blob_desc(DOCKER_MANIFEST, &digest, size);
        let tags = m.repo_tags.unwrap_or_default();
        if tags.is_empty() {
            index.manifests.push(desc);
            continue;
        }
        for tag in &tags {
            let mut r =
                Reference::parse_normalized(tag).map_err(|e| format!("normalize image ref {tag:?}: {e}"))?;
            if r.tag.is_none() && r.digest.is_none() {
                r.tag = Some("latest".into());
            }
            let mut named = desc.clone();
            named
                .annotations
                .insert("io.containerd.image.name".into(), r.to_string());
            if let Some(t) = &r.tag {
                named
                    .annotations
                    .insert("org.opencontainers.image.ref.name".into(), t.clone());
            }
            index.manifests.push(named);
        }
    }
    Ok((index, written))
}

/// The compressed layers here, by their DiffIDs: what containerd finds by the
/// `containerd.io/uncompressed` label of content it holds.
fn compressed_layers(store: &Store) -> BTreeMap<Digest, (Digest, u64)> {
    let mut found = BTreeMap::new();
    for image in store.images().unwrap_or_default() {
        let Some(config) = image.config.as_deref().and_then(|c| oci::parse_config(c).ok()) else {
            continue;
        };
        let desc = blob_desc(oci::media::OCI_MANIFEST, &image.manifest, 0);
        let Ok(bytes) = std::fs::read(store.blob_path(&image.manifest)) else {
            continue;
        };
        let Ok(oci::Document::Manifest(m)) = oci::parse_document(&bytes, &desc.media_type) else {
            continue;
        };
        for (layer, diff_id) in m.layers.iter().zip(&config.rootfs.diff_ids) {
            let (Ok(blob), Ok(diff_id)) = (layer.digest(), Digest::parse(diff_id)) else {
                continue;
            };
            if blob != diff_id && store.has(&blob) {
                found.insert(diff_id, (blob, u64::try_from(layer.size).unwrap_or(0)));
            }
        }
    }
    found
}

fn blob_desc(media_type: &str, digest: &Digest, size: u64) -> Descriptor {
    Descriptor {
        media_type: media_type.into(),
        digest: digest.to_string(),
        size: i64::try_from(size).unwrap_or(i64::MAX),
        platform: None,
        annotations: BTreeMap::new(),
    }
}

/// Client.Import's images of `index`: each entry by its name (`io.containerd.image.name`,
/// else `org.opencontainers.image.ref.name`), and by its digest when it has no name that
/// is a reference.
fn named(index: &oci::Index) -> Vec<Named> {
    let mut out = Vec::new();
    for m in &index.manifests {
        let given = m
            .annotations
            .get("io.containerd.image.name")
            .or_else(|| m.annotations.get("org.opencontainers.image.ref.name"))
            .cloned()
            .unwrap_or_default();
        let reference = Reference::parse_normalized(&given).ok();
        if let Some(mut r) = reference {
            if r.tag.is_none() && r.digest.is_none() {
                r.tag = Some("latest".into());
            }
            out.push(Named {
                name: r.to_string(),
                target: m.clone(),
                dangling: false,
            });
            continue;
        }
        if m.annotations.contains_key("io.containerd.manifest.subject") {
            continue;
        }
        out.push(Named {
            name: format!("{}{}", super::rmi::DANGLING, m.digest),
            target: m.clone(),
            dangling: true,
        });
    }
    out
}

/// The manifest of `target` for our platform, if the archive holds one: `target`
/// itself if it is a manifest for it, or what its index offers for it.
fn ours(store: &Store, target: &Descriptor) -> Option<Descriptor> {
    let targets = shards_image::platform::guest();
    let bytes = match store.held(target, oci::MAX_MANIFEST) {
        Ok(shards_image::store::Held::Whole(b)) => b,
        _ => return None,
    };
    match oci::parse_document(&bytes, &target.media_type).ok()? {
        oci::Document::Index(index) => shards_image::platform::select(&index, &targets).cloned(),
        oci::Document::Manifest(_) => Some(target.clone()),
    }
}

/// The repository a name was pulled from, as the archive's annotations say
/// (`containerd.io/distribution.source.<registry>`): the first.
fn source(target: &Descriptor) -> Option<String> {
    target.annotations.iter().find_map(|(k, v)| {
        let host = k.strip_prefix("containerd.io/distribution.source.")?;
        let path = v.split(',').next()?;
        Some(format!("{host}/{path}"))
    })
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards load` (docker/cli runLoad; moby LoadImage): the archive the client sent,
    /// read as it comes, each image it holds recorded and unpacked, said as it is.
    pub(super) fn load(&self, asker: &super::commands::Asker, reply: &super::commands::Reply<'_>) -> u8 {
        let refuse = |said: &str| {
            reply.err(said);
            1
        };
        let Some(input) = asker.files.first() else {
            return refuse("shards: load: the client sent nothing to read");
        };
        let root = self.home.join("images");
        let store = match crate::pull::store(&self.home) {
            Ok(store) => store,
            Err(e) => return refuse(&format!("{}: {e}", root.display())),
        };
        let lease = match store.lease() {
            Ok(lease) => lease,
            Err(e) => return refuse(&e.to_string()),
        };
        let file = match input.try_clone() {
            Ok(fd) => std::fs::File::from(fd),
            Err(e) => return refuse(&e.to_string()),
        };
        let mut buffered = BufReader::with_capacity(1 << 20, file);
        let head = buffered.fill_buf().map(<[u8]>::to_vec).unwrap_or_default();
        let imported = match detect(&head) {
            Compression::None => import(&store, &mut buffered),
            Compression::Gzip => import(&store, &mut shards_image::store::gunzip(buffered)),
            Compression::Zstd => std::thread::scope(|scope| {
                // The store's zstd decoder writes: it feeds a pipe the import reads.
                let (mut from, mut to) = match std::io::pipe() {
                    Ok(pipe) => pipe,
                    Err(e) => return Err(format!("failed to decompress input tar archive: {e}")),
                };
                let decoding = scope.spawn(move || {
                    shards_image::store::decode_zstd(&mut buffered, &mut to).map_err(|e| e.to_string())
                });
                let imported = import(&store, &mut from);
                drop(from);
                match decoding.join() {
                    Ok(Err(e)) if imported.is_ok() => {
                        Err(format!("failed to decompress input tar archive: {e}"))
                    }
                    _ => imported,
                }
            }),
            Compression::Bzip2 | Compression::Xz => Err(
                "failed to decompress input tar archive: shards reads plain, gzip and zstd archives".into(),
            ),
        };
        let (index, contents) = match imported {
            Ok(imported) => imported,
            Err(e) => return refuse(&e),
        };
        let limits = shards_image::store::Limits::none();
        for image in named(&index) {
            let ours = ours(&store, &image.target);
            let resolved = match image.target.digest() {
                Ok(d) => d,
                Err(e) => return refuse(&e.to_string()),
            };
            let recorded = store.tag_from(
                &image.name,
                ours.as_ref().unwrap_or(&image.target),
                &image.target,
                &contents,
                source(&image.target).as_deref(),
            );
            if let Err(e) = recorded {
                return refuse(&e.to_string());
            }
            let shown = if image.dangling {
                format!("Loaded image ID: {}", image.target.digest)
            } else {
                Reference::parse_normalized(&image.name).map_or_else(|_| image.name.clone(), |r| r.familiar())
            };
            let unpacked = ours.as_ref().map(|desc| {
                shards_registry::pull::unpack(
                    &store,
                    &shown,
                    desc,
                    resolved.clone(),
                    &shards_image::platform::guest(),
                    &limits,
                )
            });
            if image.dangling {
                reply.out(&shown);
            } else {
                reply.out(&format!("Loaded image: {shown}"));
            }
            if let Some(Err(e)) = unpacked {
                reply.out(&format!("Error unpacking image {shown}: {e}"));
            }
        }
        drop(lease);
        // What the archive held that no image names goes with the next collection.
        self.collect.store(true, std::sync::atomic::Ordering::SeqCst);
        0
    }
}
