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
    /// eStargz: gzip, each file's chunks a member of their own, and a table of contents
    /// (D82).
    Estargz,
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
            Some("estargz") => Kind::Estargz,
            Some(t @ "zstd") => {
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

/// What a layer is compressed with, as its media type and annotations say, if gzip,
/// eStargz or nothing (`estargzType.NeedsConversion`: one with a TOC is eStargz already).
fn kind_of(layer: &Layer) -> Option<Kind> {
    if layer
        .annotations
        .contains_key(super::estargz::TOC_DIGEST.as_bytes())
    {
        return Some(Kind::Estargz);
    }
    match layer.media_type.as_slice() {
        b"application/vnd.oci.image.layer.v1.tar" | b"application/vnd.docker.image.rootfs.diff.tar" => {
            Some(Kind::Uncompressed)
        }
        b"application/vnd.oci.image.layer.v1.tar+gzip"
        | b"application/vnd.docker.image.rootfs.diff.tar.gzip" => Some(Kind::Gzip),
        _ => None,
    }
}

/// `rewrite-timestamp` (BuildKit's `rewriteRemoteWithEpoch`): every layer after the
/// base's own, from the first that is not the base's layer at its place, read and written
/// again by Go's archive/tar with each time past `epoch` set to it, then compressed.
#[derive(Debug, Clone, Copy)]
pub struct Rewrite<'a> {
    /// SOURCE_DATE_EPOCH, in seconds.
    pub epoch: i64,
    /// The base image's DiffIDs.
    pub base: &'a [Vec<u8>],
}

/// The annotation a rewritten layer's descriptor carries (converter.go
/// `labelRewrittenTimestamp`).
const REWRITTEN: &str = "buildkit/rewritten-timestamp";

/// What becomes of a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Keep,
    Compress,
    Rewrite,
}

/// The build cache's record of a layer written with a compression (and an epoch): one
/// record a layer, level, kind and epoch, which a later build of the same layer takes
/// instead of writing it again.
#[derive(serde::Serialize, serde::Deserialize)]
struct Written {
    digest: String,
    size: u64,
    /// The DiffID of what was written, where a rewrite or eStargz's TOC changed it.
    #[serde(default)]
    diff_id: Option<String>,
    /// An eStargz layer's annotations: its TOC's digest and its tar's size.
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

fn record_key(diff_id: &[u8], kind: Kind, level: Option<i32>, epoch: Option<i64>) -> String {
    let mut name = format!(
        "compressed layer\0{}\0{kind:?}\0{}",
        String::from_utf8_lossy(diff_id),
        level.map_or("default".to_string(), |l| l.to_string())
    );
    if let Some(e) = epoch {
        name.push_str(&format!("\0rewritten {e}"));
    }
    super::sha256(name.as_bytes()).hex().to_string()
}

/// `layers` as an image exporter writes them with `c` and, if given, `rewrite`: those of
/// `existing` (a base's, a named context's, an artifact's: blobs that were there before
/// the build) kept as they are unless `c.force`, every other written with `c`'s
/// compression; with `rewrite`, every layer past the base's own rewritten. On as many
/// threads as the host has cores; each written once, its record kept in the build cache.
pub fn layers(
    store: &Store,
    layers: &[Layer],
    existing: &BTreeSet<Vec<u8>>,
    c: Compression,
    rewrite: Option<Rewrite<'_>>,
    limits: &Limits,
) -> Result<Vec<Layer>, String> {
    let mut diverged = false;
    let jobs: Vec<Job> = layers
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let compress = (c.force || !existing.contains(&l.digest)) && kind_of(l) != Some(c.kind);
            let Some(r) = rewrite else {
                return if compress { Job::Compress } else { Job::Keep };
            };
            if !diverged && let Some(base) = r.base.get(i) {
                if *base == l.diff_id {
                    return if compress { Job::Compress } else { Job::Keep };
                }
                diverged = true;
            }
            Job::Rewrite
        })
        .collect();
    let todo: Vec<usize> = (0..layers.len())
        .filter(|&i| jobs.get(i).is_some_and(|j| *j != Job::Keep))
        .collect();
    let mut out = layers.to_vec();
    if todo.is_empty() {
        return Ok(out);
    }
    let epoch = rewrite.map(|r| r.epoch);
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(todo.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<(usize, Result<Layer, String>)>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| -> Result<(), String> {
        let work = || loop {
            let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(&i) = todo.get(n) else { break };
            let (Some(layer), Some(job)) = (layers.get(i), jobs.get(i)) else {
                break;
            };
            let at = if *job == Job::Rewrite { epoch } else { None };
            let r = written(store, layer, c, at, limits);
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

/// A writer that hashes what passes through it: a rewritten layer's DiffID.
struct Hashing<W> {
    inner: W,
    hash: sha2::Sha256,
}

impl<W: std::io::Write> std::io::Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        let n = self.inner.write(buf)?;
        self.hash.update(buf.get(..n).unwrap_or_default());
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// tarconverter.NewReader with converter.go's `rewriteTimestampInTarHeader`: each entry
/// read and written again by Go's archive/tar, its modification, access and change times
/// past `epoch` set to it, then the end of the archive; `out` and the DiffID of what was
/// written.
fn rewrite_into<W: std::io::Write>(
    tar: &mut dyn std::io::Read,
    out: W,
    epoch: i64,
) -> Result<(W, Digest), shards_image::Error> {
    use sha2::Digest as _;
    let io = |e: shards_archive::Error| shards_image::Error::from(std::io::Error::other(e.to_string()));
    let at = shards_archive::tar::Time::unix(epoch, 0);
    let mut reader = shards_archive::tar::Reader::new(tar);
    let mut writer = shards_archive::tar::Writer::new(Hashing {
        inner: out,
        hash: sha2::Sha256::new(),
    });
    while let Some(mut h) = reader.next_header().map_err(io)? {
        for t in [&mut h.mtime, &mut h.atime, &mut h.ctime] {
            if *t > at {
                *t = at;
            }
        }
        writer.write_header(&h).map_err(io)?;
        writer.copy_from(&mut reader).map_err(io)?;
    }
    let hashing = writer.finish().map_err(io)?;
    let digest = Digest::from_hash(
        shards_image::reference::Algorithm::Sha256,
        &hashing.hash.finalize(),
    );
    Ok((hashing.inner, digest))
}

/// `layer` written with `c`, rewritten at `epoch` if given: from the build cache's record
/// if it has one whose blob is here, else written now and recorded.
fn written(
    store: &Store,
    layer: &Layer,
    c: Compression,
    epoch: Option<i64>,
    limits: &Limits,
) -> Result<Layer, String> {
    let media_type = match c.kind {
        Kind::Gzip | Kind::Estargz => OCI_LAYER_GZIP,
        Kind::Uncompressed => OCI_LAYER,
    };
    let mut annotations: BTreeMap<Vec<u8>, Vec<u8>> = epoch
        .map(|e| (REWRITTEN.as_bytes().to_vec(), e.to_string().into_bytes()))
        .into_iter()
        .collect();
    let key = record_key(&layer.diff_id, c.kind, c.level, epoch);
    if let Some(body) = store.cache_get(&key).map_err(|e| e.to_string())?
        && let Ok(w) = serde_json::from_slice::<Written>(&body)
    {
        store.cache_used(&key).map_err(|e| e.to_string())?;
        annotations.extend(
            w.annotations
                .into_iter()
                .map(|(k, v)| (k.into_bytes(), v.into_bytes())),
        );
        return Ok(Layer {
            media_type: media_type.as_bytes().to_vec(),
            digest: w.digest.into_bytes(),
            size: w.size,
            diff_id: w
                .diff_id
                .map_or_else(|| layer.diff_id.clone(), String::into_bytes),
            annotations,
            ..layer.clone()
        });
    }
    let source = store::Layer {
        blob: Digest::parse(&String::from_utf8_lossy(&layer.digest)).map_err(|e| e.to_string())?,
        media_type: String::from_utf8_lossy(&layer.media_type).into_owned(),
        diff_id: Digest::parse(&String::from_utf8_lossy(&layer.diff_id)).map_err(|e| e.to_string())?,
    };
    let blob = store.writer().map_err(|e| e.to_string())?;
    let mut esgz: Option<super::estargz::Written> = None;
    let ((digest, size), diff_id) = store
        .with_layer_tar(&source, limits, |tar| {
            let io = shards_image::Error::from;
            let level = c.level.unwrap_or(shards_flate::DEFAULT_COMPRESSION);
            let esgz_err = |e: String| shards_image::Error::from(std::io::Error::other(e));
            match (c.kind, epoch) {
                (Kind::Estargz, None) => {
                    let (blob, w) = super::estargz::write(tar, blob, level).map_err(esgz_err)?;
                    let diff = Digest::parse(&w.diff_id).map_err(|e| esgz_err(e.to_string()))?;
                    esgz = Some(w);
                    Ok((blob.commit()?, Some(diff)))
                }
                (Kind::Estargz, Some(e)) => {
                    // The rewritten tar streamed into the eStargz writer, on a thread of
                    // its own, through a pipe.
                    let (reader, writer) = std::io::pipe().map_err(io)?;
                    let (written, rewritten) = std::thread::scope(|scope| {
                        let esgz_thread = std::thread::Builder::new()
                            .name("estargz".into())
                            .spawn_scoped(scope, move || super::estargz::write(reader, blob, level));
                        let rewritten = rewrite_into(tar, writer, e).map(|(w, d)| {
                            drop(w);
                            d
                        });
                        let written = match esgz_thread {
                            Ok(t) => t
                                .join()
                                .unwrap_or_else(|_| Err("the eStargz writer failed".into())),
                            Err(e) => Err(e.to_string()),
                        };
                        (written, rewritten)
                    });
                    let _ = rewritten?;
                    let (blob, w) = written.map_err(esgz_err)?;
                    let diff = Digest::parse(&w.diff_id).map_err(|e| esgz_err(e.to_string()))?;
                    esgz = Some(w);
                    Ok((blob.commit()?, Some(diff)))
                }
                (Kind::Gzip, None) => {
                    let mut gz = shards_flate::GzipWriter::new(blob, level).map_err(io)?;
                    std::io::copy(tar, &mut gz).map_err(io)?;
                    Ok((gz.finish().map_err(io)?.commit()?, None))
                }
                (Kind::Uncompressed, None) => {
                    let mut blob = blob;
                    std::io::copy(tar, &mut blob).map_err(io)?;
                    blob.flush().map_err(io)?;
                    Ok((blob.commit()?, None))
                }
                (Kind::Gzip, Some(e)) => {
                    let gz = shards_flate::GzipWriter::new(blob, level).map_err(io)?;
                    let (gz, diff_id) = rewrite_into(tar, gz, e)?;
                    Ok((gz.finish().map_err(io)?.commit()?, Some(diff_id)))
                }
                (Kind::Uncompressed, Some(e)) => {
                    let (mut blob, diff_id) = rewrite_into(tar, blob, e)?;
                    blob.flush().map_err(io)?;
                    Ok((blob.commit()?, Some(diff_id)))
                }
            }
        })
        .map_err(|e| e.to_string())?;
    // eStargz's annotations (`EStargzAnnotations`): its TOC's digest and its tar's size.
    let made: BTreeMap<String, String> = esgz
        .map(|w| {
            BTreeMap::from([
                (super::estargz::TOC_DIGEST.to_string(), w.toc_digest),
                (
                    super::estargz::UNCOMPRESSED_SIZE.to_string(),
                    w.uncompressed.to_string(),
                ),
            ])
        })
        .unwrap_or_default();
    annotations.extend(
        made.iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec())),
    );
    let body = serde_json::to_vec(&Written {
        digest: digest.to_string(),
        size,
        diff_id: diff_id.as_ref().map(ToString::to_string),
        annotations: made,
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
        diff_id: diff_id.map_or_else(|| layer.diff_id.clone(), |d| d.to_string().into_bytes()),
        annotations,
        ..layer.clone()
    })
}
