//! The compression of an image's layers, as BuildKit's exporters apply it (D75):
//! `compression` (gzip by default), `compression-level` and `force-compression`
//! (util/compression ParseAttributes), each layer the build made written so, and with
//! `force-compression` every other too; gzip by Go's own compress/gzip
//! (`shards_flate`), so that a layer, its manifest and the image's ID are Docker's.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use shards_dockerfile::export::Layer;
use shards_image::reference::Digest;
use shards_image::store::{self, Limits, Store};

const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// What a layer is compressed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Uncompressed,
    Gzip,
}

/// An exporter's `compression`, `compression-level` and `force-compression`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compression {
    pub kind: Kind,
    pub level: Option<i32>,
    pub force: bool,
}

impl Default for Compression {
    /// BuildKit's: gzip at Go's default level, new layers only.
    fn default() -> Compression {
        Compression {
            kind: Kind::Gzip,
            level: None,
            force: false,
        }
    }
}

impl Compression {
    /// `ParseAttributes`, in its words.
    pub fn of(attrs: &BTreeMap<String, String>) -> Result<Compression, String> {
        let kind = match attrs.get("compression").map(String::as_str) {
            None | Some("gzip") => Kind::Gzip,
            Some("uncompressed") => Kind::Uncompressed,
            Some(t @ ("estargz" | "zstd")) => {
                return Err(format!("shards does not write {t} layers yet"));
            }
            Some(t) => return Err(format!("unsupported compression type {t}")),
        };
        let force = match attrs.get("force-compression").map(String::as_str) {
            None => false,
            Some("") => true,
            Some(v) => shards_cmdline::go::parse_bool(v)
                .map_err(|e| format!("non-bool value {v} specified for force-compression: {e}"))?,
        };
        let level = match attrs.get("compression-level") {
            None => None,
            Some(v) => Some(
                shards_cmdline::go::parse_int10(v)
                    .ok()
                    .and_then(|l| i32::try_from(l).ok())
                    .ok_or_else(|| {
                        format!(
                            "non-integer value {v} specified for compression-level: strconv.ParseInt: parsing {}: invalid syntax",
                            shards_cmdline::go::quote(v)
                        )
                    })?,
            ),
        };
        Ok(Compression { kind, level, force })
    }
}

/// What a layer's media type says it is compressed with, if it is gzip or nothing.
fn kind_of(media_type: &[u8]) -> Option<Kind> {
    match media_type {
        b"application/vnd.oci.image.layer.v1.tar" | b"application/vnd.docker.image.rootfs.diff.tar" => {
            Some(Kind::Uncompressed)
        }
        b"application/vnd.oci.image.layer.v1.tar+gzip"
        | b"application/vnd.docker.image.rootfs.diff.tar.gzip" => Some(Kind::Gzip),
        _ => None,
    }
}

/// The build cache's record of a layer written with a compression: one record a layer,
/// level and kind, which a later build of the same layer takes instead of compressing
/// it again.
#[derive(serde::Serialize, serde::Deserialize)]
struct Written {
    digest: String,
    size: u64,
}

fn record_key(diff_id: &[u8], kind: Kind, level: Option<i32>) -> String {
    let name = format!(
        "compressed layer\0{}\0{kind:?}\0{}",
        String::from_utf8_lossy(diff_id),
        level.map_or("default".to_string(), |l| l.to_string())
    );
    super::sha256(name.as_bytes()).hex().to_string()
}

/// `layers` as an image exporter writes them with `c`: those of `existing` (a base's, a
/// named context's, an artifact's: blobs that were there before the build) kept as they
/// are unless `c.force`, every other written with `c`'s compression, on as many threads
/// as the host has cores; each written once, its record kept in the build cache.
pub fn layers(
    store: &Store,
    layers: &[Layer],
    existing: &BTreeSet<Vec<u8>>,
    c: Compression,
    limits: &Limits,
) -> Result<Vec<Layer>, String> {
    let todo: Vec<usize> = layers
        .iter()
        .enumerate()
        .filter(|(_, l)| (c.force || !existing.contains(&l.digest)) && kind_of(&l.media_type) != Some(c.kind))
        .map(|(i, _)| i)
        .collect();
    let mut out = layers.to_vec();
    if todo.is_empty() {
        return Ok(out);
    }
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(todo.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<(usize, Result<Layer, String>)>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| -> Result<(), String> {
        let work = || loop {
            let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(&i) = todo.get(n) else { break };
            let Some(layer) = layers.get(i) else { break };
            let r = written(store, layer, c, limits);
            results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((i, r));
        };
        for _ in 1..threads {
            // One that cannot start leaves its share to the others.
            let _ = std::thread::Builder::new()
                .name("compress".into())
                .spawn_scoped(scope, work);
        }
        // This thread works too, so that no layer waits for a thread that could not start.
        work();
        Ok(())
    })?;
    let results = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (i, r) in results {
        if let Some(slot) = out.get_mut(i) {
            *slot = r?;
        }
    }
    Ok(out)
}

/// `layer` written with `c`: from the build cache's record if it has one whose blob is
/// here, else compressed now and recorded.
fn written(store: &Store, layer: &Layer, c: Compression, limits: &Limits) -> Result<Layer, String> {
    let media_type = match c.kind {
        Kind::Gzip => OCI_LAYER_GZIP,
        Kind::Uncompressed => OCI_LAYER,
    };
    let key = record_key(&layer.diff_id, c.kind, c.level);
    if let Some(body) = store.cache_get(&key).map_err(|e| e.to_string())?
        && let Ok(w) = serde_json::from_slice::<Written>(&body)
    {
        store.cache_used(&key).map_err(|e| e.to_string())?;
        return Ok(Layer {
            media_type: media_type.as_bytes().to_vec(),
            digest: w.digest.into_bytes(),
            size: w.size,
            annotations: BTreeMap::new(),
            ..layer.clone()
        });
    }
    let source = store::Layer {
        blob: Digest::parse(&String::from_utf8_lossy(&layer.digest)).map_err(|e| e.to_string())?,
        media_type: String::from_utf8_lossy(&layer.media_type).into_owned(),
        diff_id: Digest::parse(&String::from_utf8_lossy(&layer.diff_id)).map_err(|e| e.to_string())?,
    };
    let blob = store.writer().map_err(|e| e.to_string())?;
    let (digest, size) = store
        .with_layer_tar(&source, limits, |tar| {
            let io = shards_image::Error::from;
            match c.kind {
                Kind::Gzip => {
                    let level = c.level.unwrap_or(shards_flate::DEFAULT_COMPRESSION);
                    let mut gz = shards_flate::GzipWriter::new(blob, level).map_err(io)?;
                    std::io::copy(tar, &mut gz).map_err(io)?;
                    gz.finish().map_err(io)?.commit()
                }
                Kind::Uncompressed => {
                    let mut blob = blob;
                    std::io::copy(tar, &mut blob).map_err(io)?;
                    blob.flush().map_err(io)?;
                    blob.commit()
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(&Written {
        digest: digest.to_string(),
        size,
    })
    .map_err(|e| e.to_string())?;
    store
        .cache_put(
            &key,
            std::slice::from_ref(&digest),
            size,
            &String::from_utf8_lossy(&body),
        )
        .map_err(|e| e.to_string())?;
    Ok(Layer {
        media_type: media_type.as_bytes().to_vec(),
        digest: digest.to_string().into_bytes(),
        size,
        annotations: BTreeMap::new(),
        ..layer.clone()
    })
}
