//! Build caches kept outside the store (D62): what `--cache-to` writes of the build
//! cache's records (cache.rs), to a directory, a registry or the image itself, and what
//! `--cache-from` reads back, as BuildKit's remote caches carry its own.
//!
//! A cache is an OCI image manifest: its config, of type [`CONFIG`], holds each record's
//! body by its key; its layers are every layer a record names. A directory holds it as an
//! OCI layout, found by its `index.json`'s `org.opencontainers.image.ref.name` (its `tag`,
//! `latest` unless given), as BuildKit's client finds its own (client/solve.go,
//! `parseCacheOptions`); a registry by its reference. Inline, the image's config carries
//! the records whose layers are the image's own, under [`INLINE`].
//!
//! The records are shards' own: its keys are not BuildKit's, so neither can use the
//! other's, and none is passed off as BuildKit's (`application/vnd.buildkit.cacheconfig.v0`).
//! A cache that cannot be read is skipped, as BuildKit skips one (solver/llbsolver/
//! bridge.go): the build runs what it would have taken.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use shards_cmdline::buildflags::CacheEntry;
use shards_dockerfile::export::Layer;
use shards_image::oci::{self, Descriptor, Document};
use shards_image::reference::{Digest, Reference};
use shards_image::store::Store;
use shards_registry::registry::Registry;

use super::{Progress, cache, s3, show};

/// A cache's config: its records.
pub const CONFIG: &str = "application/vnd.shards.buildcache.config.v1+json";
/// The image config's field an inline cache is in.
pub const INLINE: &str = "vnd.shards.buildcache.v1";
const REF_NAME: &str = "org.opencontainers.image.ref.name";

/// Where an imported record's layers are.
enum Source {
    Dir(PathBuf),
    Registry(Box<Registry>),
    S3(Box<s3::Bucket>),
}

/// The records `--cache-from` found: each body by its key, with where its layers are.
pub struct Imported {
    records: HashMap<String, (String, usize)>,
    sources: Vec<Source>,
}

fn warn(text: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "WARNING: {text}");
}

/// A cache's records: each body by its key.
type Records = Vec<(String, String)>;

/// A cache config's records, by key: `{"records": {KEY: BODY}}`.
fn records_of(config: &[u8]) -> Result<Vec<(String, String)>, String> {
    let v: serde_json::Value = serde_json::from_slice(config).map_err(|e| e.to_string())?;
    let records = v
        .get("records")
        .and_then(serde_json::Value::as_object)
        .ok_or("a cache config without records")?;
    records
        .iter()
        .map(|(k, body)| Ok((k.clone(), serde_json::to_string(body).map_err(|e| e.to_string())?)))
        .collect()
}

/// What a fetched document holds of a cache: a cache manifest's config, or an image's
/// inline records (an index's manifest for this platform first).
fn cache_config(
    bytes: Vec<u8>,
    media_type: &str,
    fetch_doc: &dyn Fn(&Descriptor) -> Result<Vec<u8>, String>,
    fetch_blob: &dyn Fn(&Descriptor) -> Result<Vec<u8>, String>,
) -> Result<Vec<(String, String)>, String> {
    let mut bytes = bytes;
    let mut kind = media_type.to_string();
    if let Document::Index(index) = oci::parse_document(&bytes, &kind).map_err(|e| e.to_string())? {
        let chosen = shards_image::platform::select(&index, &shards_image::platform::guest())
            .ok_or("no manifest for this platform")?
            .clone();
        bytes = fetch_doc(&chosen)?;
        kind = chosen.media_type;
    }
    let Document::Manifest(m) = oci::parse_document(&bytes, &kind).map_err(|e| e.to_string())? else {
        return Err("an index inside an index".into());
    };
    let config = fetch_blob(&m.config)?;
    if m.config.media_type == CONFIG {
        return records_of(&config);
    }
    // An image: its inline records, if it has them.
    let v: serde_json::Value = serde_json::from_slice(&config).map_err(|e| e.to_string())?;
    match v.get(INLINE) {
        Some(inline) => records_of(&serde_json::to_vec(inline).map_err(|e| e.to_string())?),
        None => Err("no cache in it".into()),
    }
}

/// The OCI layout in `dir`'s manifest tagged `tag`, as `ociindex.Get` finds it.
fn tagged(dir: &Path, tag: &str) -> Result<Option<Descriptor>, String> {
    let path = dir.join("index.json");
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let index: oci::Index = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(index
        .manifests
        .into_iter()
        .find(|m| m.annotations.get(REF_NAME).map(String::as_str) == Some(tag)))
}

fn blob_in(dir: &Path, d: &Digest) -> PathBuf {
    dir.join("blobs").join(d.algorithm().name()).join(d.hex())
}

/// A blob of an OCI layout, whole and checked against its digest.
fn read_blob(dir: &Path, desc: &Descriptor) -> Result<Vec<u8>, String> {
    let d = desc.digest().map_err(|e| e.to_string())?;
    let bytes = std::fs::read(blob_in(dir, &d)).map_err(|e| format!("{d}: {e}"))?;
    if super::sha256(&bytes) != d {
        return Err(format!("{d}: its content is not its digest's"));
    }
    Ok(bytes)
}

impl Imported {
    /// Reads every `--cache-from` entry: a directory's or registry's cache, or an
    /// image's inline one. One that cannot be read is said and skipped.
    pub fn read(
        entries: &[CacheEntry],
        store: &Store,
        progress: &RefCell<Progress>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Imported, String> {
        let mut imported = Imported {
            records: HashMap::new(),
            sources: Vec::new(),
        };
        let limits = crate::pull::limits()?;
        for e in entries {
            let found = match e.kind.as_str() {
                "local" => {
                    let dir = e.attrs.get("src").filter(|s| !s.is_empty());
                    let dir = PathBuf::from(dir.ok_or("local cache importer requires src")?);
                    let tag = e.attrs.get("tag").map_or("latest", String::as_str);
                    if tag.is_empty() && !e.attrs.contains_key("digest") {
                        return Err(
                            "local cache importer requires either explicit digest, \"latest\" tag or custom tag on index.json"
                                .into(),
                        );
                    }
                    let at = format!("local cache import at {}", dir.display());
                    let desc = match e.attrs.get("digest") {
                        Some(d) => Descriptor {
                            media_type: oci::media::OCI_MANIFEST.into(),
                            digest: d.clone(),
                            size: 0,
                            platform: None,
                            annotations: BTreeMap::new(),
                        },
                        None => match tagged(&dir, tag) {
                            Ok(Some(d)) => d,
                            Ok(None) => {
                                warn(&format!("{at} skipped: no digest found for tag {tag}"));
                                continue;
                            }
                            Err(why) => {
                                warn(&format!("{at} skipped due to err: {why}"));
                                continue;
                            }
                        },
                    };
                    let read = |d: &Descriptor| read_blob(&dir, d);
                    match read(&desc).and_then(|bytes| cache_config(bytes, &desc.media_type, &read, &read)) {
                        Ok(records) => Some((records, Source::Dir(dir))),
                        Err(why) => {
                            warn(&format!("{at} skipped: {why}"));
                            None
                        }
                    }
                }
                "registry" => {
                    let name = e.attrs.get("ref").filter(|s| !s.is_empty());
                    let name = name.ok_or("registry cache importer requires ref")?.clone();
                    let v = progress
                        .borrow_mut()
                        .start(&format!("importing cache manifest from {name}"));
                    let got = (|| -> Result<(Vec<(String, String)>, Registry), String> {
                        let reference = Reference::parse(&name).map_err(|e| e.to_string())?;
                        let registry = crate::pull::registry(&reference, None, env)?;
                        let desc = registry.resolve(store, &reference).map_err(|e| e.to_string())?;
                        let doc =
                            |d: &Descriptor| registry.fetch_document(store, d).map_err(|e| e.to_string());
                        let blob = |d: &Descriptor| -> Result<Vec<u8>, String> {
                            let digest = d.digest().map_err(|e| e.to_string())?;
                            if !store.has(&digest) {
                                registry
                                    .fetch_blob(store, d, &limits, &|_| {})
                                    .map_err(|e| e.to_string())?;
                            }
                            std::fs::read(store.blob_path(&digest)).map_err(|e| e.to_string())
                        };
                        let records = cache_config(doc(&desc)?, &desc.media_type, &doc, &blob)?;
                        Ok((records, registry))
                    })();
                    match got {
                        Ok((records, registry)) => {
                            progress.borrow().done(&v);
                            Some((records, Source::Registry(Box::new(registry))))
                        }
                        // As BuildKit's: the vertex fails, the build goes on.
                        Err(why) => {
                            progress.borrow().error(&v, &why);
                            None
                        }
                    }
                }
                "s3" => {
                    // A bucket without the cache is an empty cache, as BuildKit's
                    // `getManifest` has it; one that cannot be read is skipped.
                    match s3_records(e, &limits, env) {
                        Ok(Some((records, bucket))) => Some((records, Source::S3(Box::new(bucket)))),
                        Ok(None) => None,
                        Err(why) => {
                            warn(&format!("s3 cache import skipped: {why}"));
                            None
                        }
                    }
                }
                "gha" | "azblob" => {
                    warn(&format!(
                        "shards does not read the {} cache backend yet: skipped",
                        e.kind
                    ));
                    None
                }
                other => {
                    warn(&format!(
                        "unknown cache importer: {}",
                        shards_cmdline::go::quote(other)
                    ));
                    None
                }
            };
            if let Some((records, source)) = found {
                let at = imported.sources.len();
                imported.sources.push(source);
                for (k, body) in records {
                    imported.records.entry(k).or_insert((body, at));
                }
            }
        }
        Ok(imported)
    }

    /// Takes record `key` into the store, its layers fetched where they are not here:
    /// whether it could. One whose layers cannot be had is not taken: the step runs.
    pub fn take(&self, key: &str, store: &Store) -> Result<bool, String> {
        let Some((body, at)) = self.records.get(key) else {
            return Ok(false);
        };
        let Some(source) = self.sources.get(*at) else {
            return Ok(false);
        };
        let outputs = cache::decode(body.as_bytes())?;
        let limits = crate::pull::limits()?;
        let mut blobs = Vec::new();
        for l in outputs.iter().flatten() {
            let desc = Descriptor {
                media_type: show(&l.media_type),
                digest: show(&l.digest),
                size: i64::try_from(l.size).map_err(|e| e.to_string())?,
                platform: None,
                annotations: BTreeMap::new(),
            };
            let d = desc.digest().map_err(|e| e.to_string())?;
            if !store.has(&d) {
                let fetched = match source {
                    Source::Dir(dir) => std::fs::File::open(blob_in(dir, &d))
                        .map_err(|e| e.to_string())
                        .and_then(|mut f| store.ingest(&d, l.size, &mut f).map_err(|e| e.to_string()))
                        .map(drop),
                    Source::Registry(r) => r
                        .fetch_blob(store, &desc, &limits, &|_| {})
                        .map_err(|e| e.to_string()),
                    Source::S3(b) => match b.get(&b.blob_key(&desc.digest)) {
                        Ok(Some(mut r)) => store
                            .ingest(&d, l.size, &mut r)
                            .map_err(|e| e.to_string())
                            .map(drop),
                        Ok(None) => Err("not in the bucket".into()),
                        Err(why) => Err(why),
                    },
                };
                if let Err(why) = fetched {
                    warn(&format!(
                        "the cache's layer {d} could not be had, so its step runs: {why}"
                    ));
                    return Ok(false);
                }
            }
            blobs.push(d);
        }
        let own = outputs.first().and_then(|o| o.last()).map_or(0, |l| l.size);
        store
            .cache_put(key, &blobs, own, body)
            .map_err(|e| e.to_string())?;
        Ok(true)
    }
}

/// `mode=max` exports every record of the build, `min` (the default, and what an unknown
/// mode is, as BuildKit's `parseCacheExportMode` has it) those whose layers the image
/// holds.
fn max(e: &CacheEntry) -> bool {
    e.attrs.get("mode").map(String::as_str) == Some("max")
}

/// The records of `keys` an export takes, by key, with their layers' descriptors.
fn chosen(
    keys: &[String],
    store: &Store,
    image: &BTreeSet<String>,
    all: bool,
) -> Result<(BTreeMap<String, serde_json::Value>, Vec<Descriptor>), String> {
    let mut records = BTreeMap::new();
    let mut layers: Vec<Descriptor> = Vec::new();
    for k in keys {
        if records.contains_key(k) {
            continue;
        }
        let Some(body) = store.cache_get(k).map_err(|e| e.to_string())? else {
            continue;
        };
        let outputs = cache::decode(&body)?;
        if !all && !outputs.iter().flatten().all(|l| image.contains(&show(&l.digest))) {
            continue;
        }
        for l in outputs.iter().flatten() {
            let digest = show(&l.digest);
            if !layers.iter().any(|d| d.digest == digest) {
                layers.push(Descriptor {
                    media_type: show(&l.media_type),
                    digest,
                    size: i64::try_from(l.size).map_err(|e| e.to_string())?,
                    platform: None,
                    annotations: BTreeMap::new(),
                });
            }
        }
        records.insert(
            k.clone(),
            serde_json::from_slice(&body).map_err(|e| e.to_string())?,
        );
    }
    Ok((records, layers))
}

/// The image config `config` with the records an inline cache carries: those of `keys`
/// whose layers are the image's own, in [`INLINE`], every other byte as it was. Each
/// layer is named as the image holds it (`held`, by the digest the records name it by:
/// compressed, D75), so that an importer fetches what the image pushed.
pub fn inline(
    config: &[u8],
    keys: &[String],
    store: &Store,
    image: &BTreeSet<String>,
    held: &BTreeMap<Vec<u8>, Layer>,
) -> Result<Vec<u8>, String> {
    let (mut records, _) = chosen(keys, store, image, false)?;
    for record in records.values_mut() {
        for layer in record
            .as_array_mut()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_array_mut)
            .flatten()
        {
            let Some(h) = layer
                .get("digest")
                .and_then(serde_json::Value::as_str)
                .and_then(|d| held.get(d.as_bytes()))
            else {
                continue;
            };
            layer["mediaType"] = String::from_utf8_lossy(&h.media_type).into_owned().into();
            layer["digest"] = String::from_utf8_lossy(&h.digest).into_owned().into();
            layer["size"] = h.size.into();
        }
    }
    let value = serde_json::json!({ "records": records });
    let end = config
        .iter()
        .rposition(|&b| b == b'}')
        .ok_or("an image config that is no JSON object")?;
    let (head, tail) = config.split_at(end);
    let mut out = head.to_vec();
    out.extend_from_slice(format!(",{}:", serde_json::Value::from(INLINE)).as_bytes());
    out.extend_from_slice(&serde_json::to_vec(&value).map_err(|e| e.to_string())?);
    out.extend_from_slice(tail);
    Ok(out)
}

/// What BuildKit refuses of `--cache-to` before it builds (client/solve.go
/// `parseCacheOptions`, control.go): a directory without `dest`, a registry without
/// `ref`, a bucket its attributes do not name or no credentials sign for, a backend it
/// does not know; and what shards does not write yet.
pub fn check(entries: &[CacheEntry], env: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    for e in entries {
        let set = |k: &str| e.attrs.get(k).is_some_and(|v| !v.is_empty());
        match e.kind.as_str() {
            "local" if !set("dest") => return Err("local cache exporter requires dest".into()),
            "registry" if !set("ref") => return Err("registry cache exporter requires ref".into()),
            "local" | "registry" | "inline" => {}
            "s3" => drop(s3::Bucket::of(e, env)?),
            "gha" | "azblob" => {
                return Err(format!("shards does not write the {} cache backend yet", e.kind));
            }
            other => {
                return Err(format!(
                    "unknown cache exporter: {}",
                    shards_cmdline::go::quote(other)
                ));
            }
        }
    }
    Ok(())
}

/// Writes every `--cache-to` entry but `inline`: the records of `keys` its mode takes,
/// `image` the image's layers. A failure fails the build, unless its `ignore-error`.
pub fn export(
    entries: &[CacheEntry],
    keys: &[String],
    store: &Store,
    image: &BTreeSet<String>,
    progress: &RefCell<Progress>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    check(entries, env)?;
    for e in entries.iter().filter(|e| e.kind != "inline") {
        let name = match e.kind.as_str() {
            "local" => "exporting cache to client directory",
            "s3" => "exporting cache to Amazon S3",
            _ => "exporting cache to registry",
        };
        let v = progress.borrow_mut().start(name);
        match write(e, keys, store, image, progress, &v, env) {
            Ok(()) => progress.borrow().done(&v),
            Err(why) => {
                progress.borrow().error(&v, &why);
                let ignore = e
                    .attrs
                    .get("ignore-error")
                    .is_some_and(|b| shards_cmdline::go::parse_bool(b).unwrap_or(false));
                if !ignore {
                    return Err(format!("failed to solve: {why}"));
                }
            }
        }
    }
    Ok(())
}

fn write(
    e: &CacheEntry,
    keys: &[String],
    store: &Store,
    image: &BTreeSet<String>,
    progress: &RefCell<Progress>,
    v: &super::Vertex,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let (records, layers) = chosen(keys, store, image, max(e))?;
    progress.borrow().line(v, "preparing build cache for export done");
    let config = serde_json::to_vec(&serde_json::json!({ "records": records })).map_err(|e| e.to_string())?;
    if e.kind == "s3" {
        return write_s3(e, &config, &layers, store, progress, v, env);
    }
    let config_digest = super::sha256(&config);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": oci::media::OCI_MANIFEST,
        "config": {
            "mediaType": CONFIG,
            "digest": config_digest.to_string(),
            "size": config.len(),
        },
        "layers": layers,
    }))
    .map_err(|e| e.to_string())?;
    let manifest_digest = super::sha256(&manifest);
    let desc = Descriptor {
        media_type: oci::media::OCI_MANIFEST.into(),
        digest: manifest_digest.to_string(),
        size: i64::try_from(manifest.len()).map_err(|e| e.to_string())?,
        platform: None,
        annotations: BTreeMap::new(),
    };
    match e.kind.as_str() {
        "local" => {
            let dest = PathBuf::from(e.attrs.get("dest").map_or("", String::as_str));
            let reset = match e.attrs.get("reset") {
                Some(b) => shards_cmdline::go::parse_bool(b)
                    .map_err(|err| format!("failed to parse reset attribute: {err}"))?,
                None => false,
            };
            if reset {
                for gone in [dest.join("blobs"), dest.join("index.json")] {
                    match std::fs::remove_dir_all(&gone).or_else(|_| std::fs::remove_file(&gone)) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => return Err(format!("{}: {err}", gone.display())),
                    }
                }
            }
            let put =
                |d: &Digest, from: &mut dyn FnMut(&Path) -> std::io::Result<()>| -> Result<(), String> {
                    let to = blob_in(&dest, d);
                    if to.exists() {
                        return Ok(());
                    }
                    if let Some(dir) = to.parent() {
                        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
                    }
                    from(&to).map_err(|err| format!("{}: {err}", to.display()))
                };
            for l in &layers {
                let d = l.digest().map_err(|e| e.to_string())?;
                put(&d, &mut |to| std::fs::copy(store.blob_path(&d), to).map(drop))?;
                progress.borrow().line(v, &format!("writing layer {d} done"));
            }
            put(&config_digest, &mut |to| std::fs::write(to, &config))?;
            progress
                .borrow()
                .line(v, &format!("writing config {config_digest} done"));
            put(&manifest_digest, &mut |to| std::fs::write(to, &manifest))?;
            // The index: this tag's manifest, in place of the one it named.
            let tag = e.attrs.get("tag").map_or("latest", String::as_str);
            let index_path = dest.join("index.json");
            let mut manifests: Vec<Descriptor> = match std::fs::read(&index_path) {
                Ok(b) => {
                    serde_json::from_slice::<oci::Index>(&b)
                        .map_err(|err| format!("{}: {err}", index_path.display()))?
                        .manifests
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(err) => return Err(format!("{}: {err}", index_path.display())),
            };
            manifests.retain(|m| m.annotations.get(REF_NAME).map(String::as_str) != Some(tag));
            let mut tagged = desc;
            tagged.annotations.insert(REF_NAME.into(), tag.into());
            manifests.push(tagged);
            let index = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": oci::media::OCI_INDEX,
                "manifests": manifests,
            });
            std::fs::write(
                &index_path,
                serde_json::to_vec(&index).map_err(|e| e.to_string())?,
            )
            .map_err(|err| format!("{}: {err}", index_path.display()))?;
            let layout = dest.join("oci-layout");
            if !layout.exists() {
                std::fs::write(&layout, br#"{"imageLayoutVersion":"1.0.0"}"#)
                    .map_err(|err| format!("{}: {err}", layout.display()))?;
            }
            progress
                .borrow()
                .line(v, &format!("writing cache image manifest {manifest_digest} done"));
        }
        _ => {
            let name = e.attrs.get("ref").map_or("", String::as_str);
            let reference = Reference::parse(name).map_err(|e| e.to_string())?;
            let registry = crate::pull::registry_for_push(&reference, None, None, env)?;
            store
                .ingest(&config_digest, config.len() as u64, &mut config.as_slice())
                .map_err(|e| e.to_string())?;
            store
                .ingest(&manifest_digest, manifest.len() as u64, &mut manifest.as_slice())
                .map_err(|e| e.to_string())?;
            // Each layer as the push has it; told after, the progress being this thread's.
            let pushed = std::sync::Mutex::new(Vec::new());
            shards_registry::push::push(
                &registry,
                store,
                &desc,
                Some(reference.tag.as_deref().unwrap_or("latest")),
                None,
                &|d, _| {
                    pushed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(d.to_string());
                },
            )
            .map_err(|e| e.to_string())?;
            for d in pushed
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                progress.borrow().line(v, &format!("writing layer {d} done"));
            }
            progress
                .borrow()
                .line(v, &format!("writing config {config_digest} done"));
            progress
                .borrow()
                .line(v, &format!("writing cache image manifest {manifest_digest} done"));
        }
    }
    Ok(())
}

/// The records of the s3 cache `e` names, read from its first name, with the bucket its
/// layers are in; `None` where the bucket has no such cache.
fn s3_records(
    e: &CacheEntry,
    limits: &shards_image::store::Limits,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<(Records, s3::Bucket)>, String> {
    use std::io::Read as _;
    let bucket = s3::Bucket::of(e, env)?;
    let name = bucket.names.first().map_or("", String::as_str);
    let key = bucket.manifest_key(name);
    let bytes = {
        let Some(r) = bucket.get(&key)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        r.take(limits.metadata.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{key}: {e}"))?;
        if bytes.len() as u64 > limits.metadata {
            return Err(format!("{key}: past SHARDS_MAX_IMAGE_METADATA"));
        }
        bytes
    };
    Ok(Some((records_of(&bytes)?, bucket)))
}

/// Writes a cache to S3 as BuildKit's exporter does (`Finalize`): each layer the bucket
/// lacks, `upload_parallelism` at once, one there touched once older than
/// `touch_refresh`; then `config`, the records, at each of `name`'s names.
fn write_s3(
    e: &CacheEntry,
    config: &[u8],
    layers: &[Descriptor],
    store: &Store,
    progress: &RefCell<Progress>,
    v: &super::Vertex,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let bucket = s3::Bucket::of(e, env)?;
    // A layer's key, and whether this wrote it.
    let one = |l: &Descriptor| -> Result<Option<String>, String> {
        let d = l.digest().map_err(|e| e.to_string())?;
        let key = bucket.blob_key(&l.digest);
        match bucket
            .head(&key)
            .map_err(|e| format!("failed to check file presence in cache: {e}"))?
        {
            Some(found) => {
                if bucket.stale(found.modified) {
                    bucket
                        .touch(&key, found.size)
                        .map_err(|e| format!("failed to touch file: {e}"))?;
                }
                Ok(None)
            }
            None => {
                let path = store.blob_path(&d);
                let file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let len = file
                    .metadata()
                    .map_err(|e| format!("{}: {e}", path.display()))?
                    .len();
                // A sha256 digest is the body's own hash: S3 checks it.
                let hash = if d.algorithm().name() == "sha256" {
                    d.hex()
                } else {
                    s3::UNSIGNED
                };
                bucket
                    .put(&key, s3::Body::File(&file, 0, len), hash)
                    .map_err(|e| format!("error writing layer blob: {e}"))?;
                Ok(Some(d.to_string()))
            }
        }
    };
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let threads = bucket.parallelism.min(layers.len());
    let done: Vec<Result<Vec<String>, String>> = std::thread::scope(|s| {
        let work = || -> Result<Vec<String>, String> {
            let mut wrote = Vec::new();
            while !failed.load(Ordering::Relaxed) {
                let Some(l) = layers.get(next.fetch_add(1, Ordering::Relaxed)) else {
                    break;
                };
                match one(l) {
                    Ok(d) => wrote.extend(d),
                    Err(why) => {
                        failed.store(true, Ordering::Relaxed);
                        return Err(why);
                    }
                }
            }
            Ok(wrote)
        };
        let spawned: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::Builder::new()
                    .name("s3-upload".into())
                    .spawn_scoped(s, work)
            })
            .collect();
        spawned
            .into_iter()
            .map(|h| match h {
                Ok(h) => h
                    .join()
                    .unwrap_or_else(|_| Err("an upload ended in a panic".into())),
                Err(e) => {
                    failed.store(true, Ordering::Relaxed);
                    Err(format!("starting an upload: {e}"))
                }
            })
            .collect()
    });
    for wrote in done {
        for d in wrote? {
            progress.borrow().line(v, &format!("writing layer {d} done"));
        }
    }
    let hash = super::sha256(config);
    for name in &bucket.names {
        bucket
            .put(&bucket.manifest_key(name), s3::Body::Bytes(config), hash.hex())
            .map_err(|e| format!("error writing manifest: {name}: {e}"))?;
    }
    Ok(())
}
