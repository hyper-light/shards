//! `shards load` as dockerd's containerd store loads an archive (moby docker-v29.3.1
//! daemon/containerd/image_exporter.go, LoadImage; containerd v2 core/images/archive
//! importer.go, ImportIndex; client/import.go, Import): every regular file of the tar
//! ingested as a blob, hashed as it streams; then the images an OCI layout's
//! `index.json` names, or those Docker's `manifest.json` describes, made into manifests
//! as containerd makes them; each recorded by its name, or kept by its digest
//! (`moby-dangling@`) if it has none; each unpacked for our platform, if it has one.
//!
//! Unlike dockerd: a name that is not a reference is not recorded as it is written (the
//! store records references); the image is kept by its digest instead. Zstd layers in
//! Docker's manifest.json get zstd's media type, where containerd v2.4.1 gives every
//! compressed layer gzip's, which no unpacker could then read.

use std::collections::BTreeMap;
use std::io::Read;

use shards_image::oci::{self, Descriptor};
use shards_image::reference::{Digest, Reference};
use shards_image::store::Store;

/// How much of an `oci-layout` or `manifest.json` is read for its JSON: containerd
/// v2.4.1's jsonLimit (core/images/archive/importer.go, onUntarJSON).
const MAX_JSON: u64 = 20 << 20;

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

/// An image the archive names, as containerd's Import makes it: a name and what it
/// resolved to.
struct Named {
    name: String,
    target: Descriptor,
    dangling: bool,
}

/// onUntarJSON: the first JSON value of the entry's first `MAX_JSON` bytes, as Go's
/// decoder reads one through a LimitReader: the rest of the entry, and anything after the
/// value, go unread. Nothing there is Go's `EOF`; a value cut short, its `unexpected EOF`.
fn untar_json<T: serde::de::DeserializeOwned>(
    tar: &mut shards_image::tar::Reader<&mut dyn Read>,
    size: u64,
) -> Result<T, String> {
    /// Keeps what is written up to its limit, and lets the rest go.
    struct Head(Vec<u8>);
    impl std::io::Write for Head {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let room = usize::try_from(MAX_JSON)
                .unwrap_or(usize::MAX)
                .saturating_sub(self.0.len());
            self.0
                .extend_from_slice(buf.get(..buf.len().min(room)).unwrap_or_default());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut head = Head(Vec::with_capacity(
        usize::try_from(size.min(MAX_JSON)).unwrap_or(0),
    ));
    tar.copy_data(&mut head).map_err(|e| go_tar(&e))?;
    match serde_json::Deserializer::from_slice(&head.0)
        .into_iter::<T>()
        .next()
    {
        None => Err("EOF".into()),
        Some(Err(e)) if e.is_eof() => Err("unexpected EOF".into()),
        Some(Err(e)) => Err(e.to_string()),
        Some(Ok(value)) => Ok(value),
    }
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
                // path.Join(path.Dir(name), link), as containerd's importer resolves one:
                // cleaned, so that a legacy archive's `../<id>/layer.tar` names its blob.
                let dir = name.rsplit_once('/').map_or("", |(d, _)| d);
                let joined = shards_dockerfile::go::join(&[dir.as_bytes(), &entry.link]);
                symlinks.push((name, String::from_utf8_lossy(&joined).into_owned()));
                continue;
            }
            shards_image::tar::Type::File => {}
            _ => continue,
        }
        match name.as_str() {
            "oci-layout" => {
                #[derive(serde::Deserialize)]
                struct Layout {
                    #[serde(rename = "imageLayoutVersion", default)]
                    version: String,
                }
                let l: Layout = untar_json(&mut tar, entry.size)
                    .map_err(|e| format!("untar oci layout \"oci-layout\": {e}"))?;
                layout = Some(l.version);
            }
            "manifest.json" => {
                docker = Some(
                    untar_json(&mut tar, entry.size)
                        .map_err(|e| format!("untar manifest \"manifest.json\": {e}"))?,
                );
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
            // resolveLayers: its media type, as containerd's DecompressStream tells it from
            // the first ten bytes.
            let mut head = Vec::with_capacity(10);
            std::fs::File::open(store.blob_path(digest))
                .and_then(|f| f.take(10).read_to_end(&mut head))
                .map_err(|e| format!("failed to resolve layers: {e}"))?;
            let kind = match shards_image::store::compression(&head) {
                shards_image::store::Compression::None => DOCKER_LAYER,
                shards_image::store::Compression::Gzip => DOCKER_LAYER_GZIP,
                shards_image::store::Compression::Zstd => OCI_LAYER_ZSTD,
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
            let r = Reference::parse_normalized(tag)
                .map_err(|e| format!("normalize image ref {tag:?}: {e}"))?
                .tag_name_only();
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
    // Each image's manifest for our platform and its config, and nothing else of it.
    for named in store.named().unwrap_or_default() {
        let Ok(shards_image::store::Held::Whole(bytes)) = store.held(&named.manifest, oci::MAX_MANIFEST)
        else {
            continue;
        };
        // As the manifest says it is, a Docker v2 one or an OCI one; one that says nothing
        // is OCI's.
        let parsed = oci::parse_document(&bytes, "")
            .or_else(|_| oci::parse_document(&bytes, oci::media::OCI_MANIFEST));
        let Ok(oci::Document::Manifest(m)) = parsed else {
            continue;
        };
        let Ok(shards_image::store::Held::Whole(config)) = store.held(&m.config, oci::MAX_CONFIG) else {
            continue;
        };
        let Ok(config) = oci::parse_config(&config) else {
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
        if let Some(r) = Reference::parse_normalized(&given)
            .ok()
            .map(Reference::tag_name_only)
        {
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
/// itself if it is a manifest for it, as its descriptor labels it or else its config says,
/// as an unpack checks it; or the best its index offers for it of what the archive holds,
/// as an index may list platforms it was saved without. An image for another platform
/// is loaded, not unpacked, and nothing is said of it, as dockerd says nothing.
fn ours(store: &Store, target: &Descriptor) -> Option<Descriptor> {
    let targets = shards_image::platform::guest();
    let held = |d: &Descriptor, limit: u64| match store.held(d, limit) {
        Ok(shards_image::store::Held::Whole(b)) => Some(b),
        _ => None,
    };
    let bytes = held(target, oci::MAX_MANIFEST)?;
    match oci::parse_document(&bytes, &target.media_type).ok()? {
        oci::Document::Index(mut index) => {
            index
                .manifests
                .retain(|d| d.digest().is_ok_and(|d| store.has(&d)));
            shards_image::platform::select(&index, &targets).cloned()
        }
        oci::Document::Manifest(m) => {
            let platform = match &target.platform {
                Some(p) => p.clone(),
                None => {
                    let config = oci::parse_config(&held(&m.config, oci::MAX_CONFIG)?).ok()?;
                    oci::Platform {
                        architecture: config.architecture,
                        os: config.os,
                        variant: config.variant,
                        ..oci::Platform::default()
                    }
                }
            };
            shards_image::platform::runs(&platform, &targets).then(|| target.clone())
        }
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
        // DecompressStream: plain, bzip2, gzip, xz or zstd, as its first bytes say.
        let imported = shards_build::archive::decompressed(file, 1 << 20)
            .map_err(|e| format!("failed to decompress input tar archive: {e}"))
            .and_then(|mut input| import(&store, &mut input));
        // What it ingested before failing goes with the next collection.
        let refuse = |said: &str| {
            self.collect.store(true, std::sync::atomic::Ordering::SeqCst);
            refuse(said)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A tar of one regular file, `name`, holding `data`.
    fn one(name: &str, data: &[u8]) -> Vec<u8> {
        let mut w = shards_image::tar::writer::Writer::new(Vec::new());
        w.header(&shards_image::tar::writer::Header {
            name: name.as_bytes().to_vec(),
            typeflag: b'0',
            mode: 0o644,
            size: i64::try_from(data.len()).unwrap_or(0),
            ..Default::default()
        })
        .unwrap();
        w.write(data).unwrap();
        w.finish().unwrap()
    }

    /// What `untar_json` makes of `data`, and whether the entry was read to its end, as
    /// the archive goes on after it.
    fn read_one(data: &[u8]) -> (Result<serde_json::Value, String>, bool) {
        let archive = one("manifest.json", data);
        let mut slice: &[u8] = &archive;
        let input: &mut dyn Read = &mut slice;
        let mut tar = shards_image::tar::Reader::new(input);
        let entry = tar.next_entry().unwrap().unwrap();
        let value = untar_json(&mut tar, entry.size);
        (value, matches!(tar.next_entry(), Ok(None)))
    }

    /// onUntarJSON (containerd v2.4.1): the first value of the first 20 MiB, what follows
    /// it unread; nothing is Go's `EOF`, and a value cut short its `unexpected EOF`.
    #[test]
    fn archive_json_is_read_as_containerd_reads_it() {
        let read = |data: &[u8]| {
            let (value, ended) = read_one(data);
            assert!(ended, "the entry read to its end");
            value
        };
        assert_eq!(
            read(br#"[{"Config":"c"}] trailing"#).unwrap(),
            serde_json::json!([{"Config": "c"}])
        );
        assert_eq!(read(b"").unwrap_err(), "EOF");
        assert_eq!(read(b"  \n").unwrap_err(), "EOF");
        assert_eq!(read(br#"[{"Config":"#).unwrap_err(), "unexpected EOF");
        // A value within the limit, the entry past it.
        let mut big = br#"{"a":1}"#.to_vec();
        big.resize(usize::try_from(MAX_JSON).unwrap() + 10, b' ');
        assert_eq!(read(&big).unwrap(), serde_json::json!({"a": 1}));
        // A value of 18 MiB: within containerd's limit.
        let mut within = br#"{"a":""#.to_vec();
        within.resize(18 << 20, b'x');
        within.extend_from_slice(br#""}"#);
        assert!(read(&within).is_ok());
        // A value past the limit: cut short.
        let mut past = br#"{"a":""#.to_vec();
        past.resize(usize::try_from(MAX_JSON).unwrap() + 10, b'x');
        past.extend_from_slice(br#""}"#);
        assert_eq!(read(&past).unwrap_err(), "unexpected EOF");
    }
}
