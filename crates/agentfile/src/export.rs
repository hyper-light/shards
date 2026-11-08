//! An image as BuildKit's image exporter writes it (`exporter/containerimage/writer.go`
//! of moby/buildkit dockerfile/1.27.1): the frontend's config with its history reconciled
//! with the layers (`normalizeLayersAndHistory`), its `rootfs`, `history` and `created`
//! patched in (`patchImageConfig`), and the OCI manifest that names it and its layers.

use std::collections::BTreeMap;

use crate::go::Time;
use crate::image::{self, History, Image};
use crate::json;

/// A layer of the image: its blob, its DiffID, and when and how it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    pub media_type: Vec<u8>,
    pub digest: Vec<u8>,
    pub size: u64,
    pub diff_id: Vec<u8>,
    /// Its descriptor's annotations, as the base's manifest has them.
    pub annotations: BTreeMap<Vec<u8>, Vec<u8>>,
    /// When it was made: for a layer of a base, when the store first held it.
    pub created: Option<Time>,
    /// What made it, for history that lacks an entry for it.
    pub description: Vec<u8>,
}

/// `normalizeLayersAndHistory`: one non-empty history entry per layer, and every entry a
/// time.
fn normalize(layers: &[Layer], mut history: Vec<History>) -> Vec<History> {
    let with_layer = history.iter().filter(|h| !h.empty_layer).count();
    if with_layer > layers.len() {
        // More entries claim layers than there are: the later ones claim none.
        let mut l = 0;
        for h in &mut history {
            if l >= layers.len() {
                h.empty_layer = true;
            }
            if !h.empty_layer {
                l += 1;
            }
        }
    }
    if layers.len() > with_layer {
        for layer in layers.iter().skip(with_layer) {
            history.push(History {
                created: layer.created,
                created_by: layer.description.clone(),
                author: Vec::new(),
                comment: b"buildkit.exporter.image.v0".to_vec(),
                empty_layer: false,
            });
        }
    }
    let mut index = 0;
    for h in &mut history {
        if !h.empty_layer {
            if h.created.is_none() {
                h.created = layers.get(index).and_then(|l| l.created);
            }
            index += 1;
        }
    }
    // An entry without a time takes the one before it, or the first after.
    let mut created: Option<Time> = None;
    let mut missing = false;
    for h in &history {
        match h.created {
            Some(t) => {
                created = Some(t);
                if missing {
                    break;
                }
            }
            None => missing = true,
        }
    }
    let mut missing = false;
    for h in &mut history {
        match h.created {
            Some(t) => {
                if missing {
                    created = Some(t);
                }
            }
            None => {
                missing = true;
                h.created = created;
            }
        }
    }
    history
}

fn after(a: &Time, b: &Time) -> bool {
    a.unix() > b.unix()
}

/// The config the exporter writes: the frontend's `image`, with `layers`' DiffIDs as its
/// `rootfs`, its history normalized and, with SOURCE_DATE_EPOCH (`epoch`), clamped to it
/// after the entries that are the `base`'s, and `created` the last entry's time. Its
/// members sorted, as Go writes a map.
pub fn config(
    image: &Image,
    layers: &[Layer],
    epoch: Option<Time>,
    base: Option<&Image>,
) -> Result<Vec<u8>, Vec<u8>> {
    if image.platform.os.is_empty() {
        return Err(b"invalid image config for export: missing os".to_vec());
    }
    if image.platform.architecture.is_empty() {
        return Err(b"invalid image config for export: missing architecture".to_vec());
    }
    let mut history = normalize(layers, image.history.clone());
    let mut members: BTreeMap<&str, String> = image.members()?.into_iter().collect();
    let diff_ids: Vec<Vec<u8>> = layers.iter().map(|l| l.diff_id.clone()).collect();
    members.insert(
        "rootfs",
        image::rootfs_json(b"layers", (!diff_ids.is_empty()).then_some(diff_ids.as_slice())),
    );
    if let Some(epoch) = &epoch {
        let mut diverged = false;
        for (i, h) in history.iter_mut().enumerate() {
            if !diverged && base.and_then(|b| b.history.get(i)) == Some(&*h) {
                continue;
            }
            diverged = true;
            if h.created.is_none_or(|c| after(&c, epoch)) {
                h.created = Some(*epoch);
            }
        }
    }
    members.insert("history", image::history_json(&history)?);
    match (members.get("created"), epoch) {
        (Some(_), Some(epoch)) => {
            if image.created.is_some_and(|c| after(&c, &epoch)) {
                members.insert("created", image::time_json(&epoch)?);
            }
        }
        (Some(_), None) => {}
        (None, _) => {
            let last = history.iter().rev().find_map(|h| h.created);
            let created = match last {
                Some(t) => image::time_json(&t)?,
                None => "null".to_string(),
            };
            members.insert("created", created);
        }
    }
    let mut out = String::from("{");
    for (i, (k, v)) in members.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json::write_string(&mut out, k.as_bytes());
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
    Ok(out.into_bytes())
}

const OCI_MANIFEST: &[u8] = b"application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &[u8] = b"application/vnd.oci.image.config.v1+json";
const DOCKER_MANIFEST: &[u8] = b"application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_CONFIG: &[u8] = b"application/vnd.docker.container.image.v1+json";

/// `s` as `json.Marshal` writes a string: `<`, `>` and `&` escaped for HTML.
pub fn json_string(s: &[u8]) -> String {
    let mut out = String::new();
    json::write_string(&mut out, s);
    out
}

/// `toDockerLayerType` (buildkit util/compression/compression.go): a layer's media type
/// as Docker names it; one it does not know as it is.
pub fn docker_layer_type(media_type: &[u8]) -> Vec<u8> {
    let docker: &[u8] = match media_type {
        b"application/vnd.oci.image.layer.v1.tar" => b"application/vnd.docker.image.rootfs.diff.tar",
        b"application/vnd.oci.image.layer.v1.tar+gzip" => {
            b"application/vnd.docker.image.rootfs.diff.tar.gzip"
        }
        b"application/vnd.oci.image.layer.v1.tar+zstd" => {
            b"application/vnd.docker.image.rootfs.diff.tar.zstd"
        }
        b"application/vnd.oci.image.layer.nondistributable.v1.tar" => {
            b"application/vnd.docker.image.rootfs.foreign.diff.tar"
        }
        b"application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => {
            b"application/vnd.docker.image.rootfs.foreign.diff.tar.gzip"
        }
        other => other,
    };
    docker.to_vec()
}

/// `toOCILayerType`: a layer's media type as OCI names it; one it does not know as it is.
pub fn oci_layer_type(media_type: &[u8]) -> Vec<u8> {
    let oci: &[u8] = match media_type {
        b"application/vnd.docker.image.rootfs.diff.tar" => b"application/vnd.oci.image.layer.v1.tar",
        b"application/vnd.docker.image.rootfs.diff.tar.gzip" => {
            b"application/vnd.oci.image.layer.v1.tar+gzip"
        }
        b"application/vnd.docker.image.rootfs.diff.tar.zstd" => {
            b"application/vnd.oci.image.layer.v1.tar+zstd"
        }
        b"application/vnd.docker.image.rootfs.foreign.diff.tar" => {
            b"application/vnd.oci.image.layer.nondistributable.v1.tar"
        }
        b"application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => {
            b"application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        }
        other => other,
    };
    oci.to_vec()
}

/// A descriptor as `json.MarshalIndent` writes an `ocispec.Descriptor`, indented by
/// `indent`.
fn descriptor(
    out: &mut String,
    indent: &str,
    media_type: &[u8],
    digest: &[u8],
    size: u64,
    annotations: &BTreeMap<Vec<u8>, Vec<u8>>,
) {
    out.push_str("{\n");
    let inner = format!("{indent}  ");
    out.push_str(&inner);
    out.push_str("\"mediaType\": ");
    json::write_string(out, media_type);
    out.push_str(",\n");
    out.push_str(&inner);
    out.push_str("\"digest\": ");
    json::write_string(out, digest);
    out.push_str(",\n");
    out.push_str(&inner);
    out.push_str(&format!("\"size\": {size}"));
    if !annotations.is_empty() {
        out.push_str(",\n");
        out.push_str(&inner);
        out.push_str("\"annotations\": {\n");
        for (i, (k, v)) in annotations.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!("{inner}  "));
            json::write_string(out, k);
            out.push_str(": ");
            json::write_string(out, v);
        }
        out.push('\n');
        out.push_str(&inner);
        out.push('}');
    }
    out.push('\n');
    out.push_str(indent);
    out.push('}');
}

/// The image's manifest as the exporter writes it, `json.MarshalIndent` with two spaces:
/// the config, then the layers with OCI's media types and their annotations but the
/// exporter's internal ones (`RemoveInternalLayerAnnotations`).
pub fn manifest(config: &[u8], config_digest: &[u8], layers: &[Layer]) -> Vec<u8> {
    manifest_as(config, config_digest, layers, false, &BTreeMap::new())
}

/// [`manifest`] with `annotations`, which `json.MarshalIndent` writes after the layers, as
/// ocispec.Manifest orders its fields; none, and it is [`manifest`]'s.
pub fn manifest_annotated(
    config: &[u8],
    config_digest: &[u8],
    layers: &[Layer],
    annotations: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
    manifest_as(config, config_digest, layers, false, annotations)
}

/// [`manifest`] in Docker's media types, as the `docker` exporter writes it (`OCITypes`
/// false, buildkit exporter/oci/export.go): the same document, the same blobs.
pub fn docker_manifest(config: &[u8], config_digest: &[u8], layers: &[Layer]) -> Vec<u8> {
    manifest_as(config, config_digest, layers, true, &BTreeMap::new())
}

fn manifest_as(
    config: &[u8],
    config_digest: &[u8],
    layers: &[Layer],
    docker: bool,
    annotations: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
    let (manifest_type, config_type) = if docker {
        (DOCKER_MANIFEST, DOCKER_CONFIG)
    } else {
        (OCI_MANIFEST, OCI_CONFIG)
    };
    let mut out = String::from("{\n  \"schemaVersion\": 2,\n  \"mediaType\": ");
    json::write_string(&mut out, manifest_type);
    out.push_str(",\n  \"config\": ");
    descriptor(
        &mut out,
        "  ",
        config_type,
        config_digest,
        config.len() as u64,
        &BTreeMap::new(),
    );
    out.push_str(",\n  \"layers\": ");
    if layers.is_empty() {
        out.push_str("null");
    } else {
        out.push_str("[\n");
        for (i, l) in layers.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
            }
            out.push_str("    ");
            let kept: BTreeMap<Vec<u8>, Vec<u8>> = l
                .annotations
                .iter()
                .filter(|(k, _)| {
                    k.as_slice() != b"containerd.io/uncompressed"
                        && k.as_slice() != b"buildkit/createdat"
                        && !k.starts_with(b"containerd.io/distribution.source.")
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let oci = oci_layer_type(&l.media_type);
            descriptor(
                &mut out,
                "    ",
                &if docker { docker_layer_type(&oci) } else { oci },
                &l.digest,
                l.size,
                &kept,
            );
        }
        out.push_str("\n  ]");
    }
    if !annotations.is_empty() {
        out.push_str(",\n  \"annotations\": {\n");
        for (i, (k, v)) in annotations.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
            }
            out.push_str("    ");
            json::write_string(&mut out, k);
            out.push_str(": ");
            json::write_string(&mut out, v);
        }
        out.push_str("\n  }");
    }
    out.push_str("\n}");
    out.into_bytes()
}
