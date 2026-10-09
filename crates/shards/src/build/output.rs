//! What `shards build --output` writes (docs/design/architecture.md D52), as BuildKit's
//! exporters write it through buildx (measured against dockerd 29.3.1, BuildKit v0.28.1,
//! buildx v0.33.0, 2026-10-06):
//!
//! - `local`: the image's whole root filesystem, received into a directory as fsutil
//!   receives it ([`shards_archive::receive`]), every entry owned by the one who builds
//!   (buildkit session/filesync/diffcopy.go).
//! - `tar`: the same tree as fsutil.WriteTar writes it, through Go's archive/tar with no
//!   format asked for: names in walk order, no root, times rounded to seconds.
//! - `oci` and `docker`: the image as an OCI layout, in a tar as containerd's exporter
//!   writes one (core/images/archive/exporter.go), or with `tar=false` in a directory as
//!   buildkit's client writes its content store (client/ociindex); `docker` adds
//!   `manifest.json` and names its manifest and layers in Docker's media types.
//!
//! The filesystem outputs are written from the build's last snapshot, which keeps the
//! nanoseconds its layers' headers drop. The image ones hold the very blobs the store
//! keeps: layers, config and manifest. A layer shards made is not compressed, which no
//! byte-for-byte copy of Go's gzip could make it.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::Path;

use shards_archive::receive::{self, Receiver};
use shards_archive::tar::{self as gotar, Header};
use shards_build::data::Sources;
use shards_build::vfs::Fs;
use shards_dockerfile::export::{self, Layer, json_string};
use shards_image::erofs::{DataRef, Kind, Node, NodeId, Source as _, Tree};
use shards_image::reference::{Digest, Reference};
use shards_image::store::Store;

/// The time an output stamps: its `source-date-epoch` attribute, else the build's
/// SOURCE_DATE_EPOCH (buildkit exporter/util/epoch).
pub fn epoch(attrs: &BTreeMap<String, String>, build: Option<i64>) -> Result<Option<i64>, String> {
    match attrs.get("source-date-epoch").map(String::as_str) {
        None => Ok(build),
        Some("") => Ok(None),
        Some(v) => v.parse::<i64>().map(Some).map_err(|e| {
            let why = match e.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                    "value out of range"
                }
                _ => "invalid syntax",
            };
            let e = format!("strconv.ParseInt: parsing {}: {why}", json_string(v.as_bytes()));
            format!("invalid source-date-epoch: {e}: {e}")
        }),
    }
}

/// Each entry under the root of `tree`, its path and node, in walk order: depth first,
/// names in byte order, as filepath.Walk goes.
fn walk(tree: &Tree, mut f: impl FnMut(&[u8], NodeId, &Node) -> Result<(), String>) -> Result<(), String> {
    let mut stack: Vec<(Vec<u8>, NodeId)> = Vec::new();
    let push = |stack: &mut Vec<(Vec<u8>, NodeId)>, path: &[u8], dir: NodeId| {
        let mut entries = tree.entries(dir);
        entries.reverse();
        for (name, child) in entries {
            let p = if path.is_empty() {
                name.to_vec()
            } else {
                [path, b"/", name].concat()
            };
            stack.push((p, child));
        }
    };
    push(&mut stack, b"", Tree::ROOT);
    while let Some((path, id)) = stack.pop() {
        let node = tree.node(id).ok_or("a snapshot names a node it lacks")?;
        f(&path, id, node)?;
        if matches!(node.kind, Kind::Dir(_)) {
            push(&mut stack, &path, id);
        }
    }
    Ok(())
}

/// A file's bytes, from where the build holds them.
fn copy(sources: &mut Sources, data: DataRef, size: u64, out: &mut dyn Write) -> io::Result<()> {
    let mut buf = vec![0u8; usize::try_from(size.min(1 << 20)).unwrap_or(1 << 20)];
    let mut at = 0u64;
    while at < size {
        let n = usize::try_from((size - at).min(buf.len() as u64)).unwrap_or(buf.len());
        let chunk = buf
            .get_mut(..n)
            .ok_or_else(|| io::Error::other("a chunk past the buffer"))?;
        sources.read_at(data, at, chunk)?;
        out.write_all(chunk)?;
        at += n as u64;
    }
    Ok(())
}

/// A node's time, or the epoch's.
fn time_of(node: &Node, epoch: Option<i64>) -> gotar::Time {
    match epoch {
        Some(sec) => gotar::Time::unix(sec, 0),
        None => gotar::Time::unix(node.meta.mtime, i64::from(node.meta.mtime_nsec)),
    }
}

/// fsutil.WriteTar of the snapshot `fs`: each entry's header from its kind, mode, owner
/// and time, as tar.FileInfoHeader makes one of fsutil's stat (no user or group names),
/// its extended attributes as `SCHILY.xattr.` records; a name met again a hard link to
/// the first. Returns `out`, the archive ended.
pub fn tar<W: Write>(fs: &Fs, sources: &mut Sources, epoch: Option<i64>, out: W) -> Result<W, String> {
    let tree = fs.tree();
    let mut tw = gotar::Writer::new(out);
    let mut seen: HashMap<NodeId, Vec<u8>> = HashMap::new();
    walk(tree, |path, id, node| {
        let mut h = Header {
            name: path.to_vec(),
            mode: i64::from(node.meta.mode & 0o7777),
            uid: i64::from(node.meta.uid),
            gid: i64::from(node.meta.gid),
            mtime: time_of(node, epoch),
            ..Header::default()
        };
        for (k, v) in node.meta.xattrs.iter() {
            h.pax.insert([b"SCHILY.xattr.".as_slice(), k].concat(), v.clone());
        }
        let mut data = None;
        match &node.kind {
            Kind::Dir(_) => {
                h.typeflag = gotar::TYPE_DIR;
                h.name.push(b'/');
            }
            _ if seen.contains_key(&id) => {
                h.typeflag = gotar::TYPE_LINK;
                h.linkname = seen.get(&id).cloned().unwrap_or_default();
            }
            Kind::File { size, data: d } => {
                h.typeflag = gotar::TYPE_REG;
                h.size = i64::try_from(*size).map_err(|e| e.to_string())?;
                data = Some((*d, *size));
            }
            Kind::Symlink(target) => {
                h.typeflag = gotar::TYPE_SYMLINK;
                h.linkname = target.to_vec();
            }
            Kind::CharDevice { major, minor } => {
                h.typeflag = gotar::TYPE_CHAR;
                (h.devmajor, h.devminor) = (i64::from(*major), i64::from(*minor));
            }
            Kind::BlockDevice { major, minor } => {
                h.typeflag = gotar::TYPE_BLOCK;
                (h.devmajor, h.devminor) = (i64::from(*major), i64::from(*minor));
            }
            Kind::Fifo => h.typeflag = gotar::TYPE_FIFO,
            Kind::Socket => return Err("archive/tar: sockets not supported".into()),
        }
        if !matches!(node.kind, Kind::Dir(_)) {
            seen.entry(id).or_insert_with(|| path.to_vec());
        }
        tw.write_header(&h).map_err(|e| {
            format!(
                "failed to write file header {}: {e}",
                String::from_utf8_lossy(&h.name)
            )
        })?;
        if let Some((d, size)) = data {
            copy(sources, d, size, &mut tw).map_err(|e| e.to_string())?;
        }
        Ok(())
    })?;
    tw.finish().map_err(|e| e.to_string())
}

/// The ids a file the build writes is owned by: the builder's own, as buildx's receive
/// filter sets them (diffcopy.go).
fn own_ids() -> (u32, u32) {
    #[cfg(unix)]
    {
        // SAFETY: getuid and getgid cannot fail and touch no memory of ours.
        unsafe { (libc::getuid(), libc::getgid()) }
    }
    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

/// The snapshot `fs` received into `dest`; with `mirror` (`mode=delete`), what `dest`
/// held that it lacks removed. Returns the bytes of its files.
pub fn local(
    fs: &Fs,
    sources: &mut Sources,
    epoch: Option<i64>,
    dest: &Path,
    mirror: bool,
) -> Result<u64, String> {
    let tree = fs.tree();
    let (uid, gid) = own_ids();
    let mut r = Receiver::create(dest, mirror).map_err(|e| e.to_string())?;
    let mut seen: HashMap<NodeId, Vec<u8>> = HashMap::new();
    let mut bytes = 0u64;
    walk(tree, |path, id, node| {
        let first = seen.get(&id).cloned();
        let kind = match (&node.kind, &first) {
            (Kind::Dir(_), _) => receive::Kind::Dir,
            (_, Some(first)) => receive::Kind::Link(first),
            (Kind::File { .. }, None) => receive::Kind::File,
            (Kind::Symlink(t), None) => receive::Kind::Symlink(t),
            (Kind::CharDevice { major, minor }, None) => receive::Kind::Char {
                major: *major,
                minor: *minor,
            },
            (Kind::BlockDevice { major, minor }, None) => receive::Kind::Block {
                major: *major,
                minor: *minor,
            },
            (Kind::Fifo, None) => receive::Kind::Fifo,
            (Kind::Socket, None) => {
                return Err(format!(
                    "{}: a socket, which no output holds",
                    String::from_utf8_lossy(path)
                ));
            }
        };
        let e = receive::Entry {
            path,
            kind,
            mode: u32::from(node.meta.mode & 0o7777),
            uid,
            gid,
            mtime: time_of(node, epoch),
            xattrs: node
                .meta
                .xattrs
                .iter()
                .map(|(k, v)| (k.as_slice(), v.as_slice()))
                .collect(),
        };
        let mut data = |w: &mut dyn Write| match &node.kind {
            Kind::File { size, data } if first.is_none() => {
                bytes += size;
                copy(sources, *data, *size, w)
            }
            _ => Ok(()),
        };
        r.put(&e, &mut data).map_err(|e| e.to_string())?;
        if !matches!(node.kind, Kind::Dir(_)) && first.is_none() {
            seen.insert(id, path.to_vec());
        }
        Ok(())
    })?;
    r.finish().map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// An image the build made, as the store holds it.
pub struct Made<'a> {
    pub config: &'a [u8],
    pub config_digest: &'a Digest,
    pub manifest: &'a [u8],
    pub manifest_digest: &'a Digest,
    pub layers: &'a [Layer],
    /// `--annotation`'s for its descriptor (`manifest-descriptor`), which the exporter's
    /// own (when it was made, its names) follow.
    pub descriptor_annotations: &'a BTreeMap<String, String>,
    /// The provenance its outputs carry, where one is asked for in every output (D72).
    pub provenance: Option<(&'a super::provenance::Capture, &'a super::provenance::Run)>,
    /// Whether its attestation holds the provenance beside its SBOMs, as a default one
    /// (`inline-only`) is held where another attestation makes the index (D81).
    pub provenance_in: bool,
}

/// An image's attestation in an OCI layout (D72): its documents, by digest, and the index
/// of the image and it, which the layout names.
struct Attested {
    blobs: Vec<(Digest, Vec<u8>)>,
    index: (Digest, usize),
}

/// The platform of an image's config, as the purl and the index name it.
fn platform_parts(config: &[u8]) -> Result<(String, String, String), String> {
    let v: serde_json::Value = serde_json::from_slice(config).map_err(|e| e.to_string())?;
    let s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let (arch, os, mut variant) = (s("architecture"), s("os"), s("variant"));
    if arch == "arm64" && variant == "v8" {
        variant.clear();
    }
    Ok((arch, os, variant))
}

/// `made`'s attestation where it carries one: its statement naming the image by each of
/// `names` and its manifest, the attestation's config and manifest, and the index.
fn attested(made: &Made<'_>, names: &[Reference]) -> Result<Option<Attested>, String> {
    use super::provenance;
    let Some((capture, run)) = made.provenance else {
        return Ok(None);
    };
    let (arch, os, variant) = platform_parts(made.config)?;
    let mut platform = format!("{os}/{arch}");
    if !variant.is_empty() {
        platform.push_str(&format!("/{variant}"));
    }
    let subjects: Vec<(String, String)> = names
        .iter()
        .map(|n| {
            (
                provenance::image_purl(n, &platform),
                made.manifest_digest.to_string(),
            )
        })
        .collect();
    let statements = provenance::image_statements(capture, run, &subjects, made.provenance_in);
    if statements.is_empty() {
        return Ok(None);
    }
    let digests: Vec<String> = statements
        .iter()
        .map(|(s, _)| super::sha256(s.as_bytes()).to_string())
        .collect();
    let listed: Vec<(&str, usize, &str)> = statements
        .iter()
        .zip(&digests)
        .map(|((s, kind), d)| (d.as_str(), s.len(), kind.as_str()))
        .collect();
    let (config, manifest) = provenance::attestation_of(&listed);
    let manifest_digest = super::sha256(manifest.as_bytes());
    let index = provenance::index(
        (&made.manifest_digest.to_string(), made.manifest.len()),
        (&arch, &os, &variant),
        (&manifest_digest.to_string(), manifest.len()),
    )
    .into_bytes();
    let index_digest = super::sha256(&index);
    let index_len = index.len();
    let mut blobs: Vec<(Digest, Vec<u8>)> = statements
        .into_iter()
        .map(|(s, _)| (super::sha256(s.as_bytes()), s.into_bytes()))
        .collect();
    blobs.push((super::sha256(config.as_bytes()), config.into_bytes()));
    blobs.push((manifest_digest, manifest.into_bytes()));
    blobs.push((index_digest.clone(), index));
    Ok(Some(Attested {
        blobs,
        index: (index_digest, index_len),
    }))
}

/// The index an OCI layout names `made` by, where it carries its provenance: its digest
/// and size.
pub fn attested_index(made: &Made<'_>, names: &[Reference]) -> Result<Option<(Digest, usize)>, String> {
    Ok(attested(made, names)?.map(|a| a.index))
}

/// A local or tar output's attestations, as BuildKit's local exporter makes them: each
/// SBOM at its file's name in the scan (D81), and `provenance.json` where `provenance`
/// (D72); each statement indented, naming each regular file `fs` holds by its path and
/// SHA-256.
pub fn attestations(
    fs: &Fs,
    sources: &mut Sources,
    capture: &super::provenance::Capture,
    run: &super::provenance::Run,
    provenance: bool,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    if capture.sboms.is_empty() && !provenance {
        return Ok(Vec::new());
    }
    use sha2::Digest as _;
    use shards_image::erofs::Source as _;
    let mut subjects = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk(fs.tree(), |path, id, node| {
        if let Kind::File { size, data } = node.kind
            && seen.insert(id)
        {
            let rel = String::from_utf8_lossy(path).trim_start_matches('/').to_string();
            let mut h = sha2::Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut at = 0u64;
            while at < size {
                let n = usize::try_from((size - at).min(buf.len() as u64)).map_err(|e| e.to_string())?;
                let chunk = buf.get_mut(..n).unwrap_or_default();
                sources
                    .read_at(data, at, chunk)
                    .map_err(|e| format!("{rel}: {e}"))?;
                h.update(&*chunk);
                at += n as u64;
            }
            let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            subjects.push((rel, format!("sha256:{hex}")));
        }
        Ok(())
    })?;
    let mut files = Vec::new();
    for s in &capture.sboms {
        let text = shards_dockerfile::json_indent(super::sbom::intoto(s, &subjects).as_bytes());
        let name = Path::new(&s.path)
            .file_name()
            .ok_or_else(|| format!("{}: no file name", s.path))?
            .to_string_lossy()
            .into_owned();
        files.push((name, text));
    }
    if provenance {
        let text = super::provenance::statement_json(capture, run, &subjects).indented(0);
        files.push(("provenance.json".to_string(), text.into_bytes()));
    }
    Ok(files)
}

/// A local output's attestations written beside its files, as BuildKit's local exporter
/// writes them: mode 0600, their time the build's SOURCE_DATE_EPOCH where it has one, else
/// now (measured, Docker 29.3.1).
pub fn local_attestations(
    dest: &Path,
    files: &[(String, Vec<u8>)],
    epoch: Option<i64>,
) -> Result<(), String> {
    for (name, text) in files {
        let path = dest.join(name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&path).map_err(|e| format!("{name}: {e}"))?;
        f.write_all(text).map_err(|e| format!("{name}: {e}"))?;
        if let Some(sec) = epoch {
            let when = u64::try_from(sec)
                .ok()
                .and_then(|s| std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(s)))
                .unwrap_or(std::time::UNIX_EPOCH);
            f.set_modified(when).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    Ok(())
}

/// `fs` with a tar output's attestations at its root, as BuildKit's tar exporter adds them
/// to what it writes: mode 0600, owner 0, their time now (the tar's SOURCE_DATE_EPOCH
/// applies to them as to every entry).
pub fn with_attestations(
    fs: &Fs,
    sources: &mut Sources,
    files: Vec<(String, Vec<u8>)>,
) -> Result<Fs, String> {
    let mut out = fs.clone();
    let (sec, nsec) = super::exec::now();
    for (name, text) in files {
        let size = text.len() as u64;
        let data = sources.bytes(text).map_err(|e| e.to_string())?;
        out.put(
            name.as_bytes(),
            Node {
                kind: Kind::File { size, data },
                meta: shards_image::erofs::Meta {
                    mode: 0o600,
                    mtime: sec,
                    mtime_nsec: nsec,
                    ..shards_image::erofs::Meta::default()
                },
            },
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// `platform` of a descriptor, as ocispec.Platform marshals: from the image's config.
fn platform(config: &[u8]) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_slice(config).map_err(|e| e.to_string())?;
    platform_of(&v)
}

fn platform_of(v: &serde_json::Value) -> Result<String, String> {
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
    let mut out = format!(
        r#"{{"architecture":{},"os":{}"#,
        json_string(s("architecture").as_bytes()),
        json_string(s("os").as_bytes())
    );
    if !s("os.version").is_empty() {
        out.push_str(&format!(
            r#","os.version":{}"#,
            json_string(s("os.version").as_bytes())
        ));
    }
    if let Some(features) = v.get("os.features").and_then(serde_json::Value::as_array)
        && !features.is_empty()
    {
        let items: Vec<String> = features
            .iter()
            .map(|f| json_string(f.as_str().unwrap_or("").as_bytes()))
            .collect();
        out.push_str(&format!(r#","os.features":[{}]"#, items.join(",")));
    }
    if !s("variant").is_empty() {
        out.push_str(&format!(r#","variant":{}"#, json_string(s("variant").as_bytes())));
    }
    out.push('}');
    Ok(out)
}

/// A descriptor as ocispec.Descriptor marshals: annotations in key order.
fn descriptor(
    media_type: &str,
    digest: &str,
    size: usize,
    annotations: &BTreeMap<String, String>,
    platform: &str,
) -> String {
    let mut out = format!(
        r#"{{"mediaType":{},"digest":{},"size":{size}"#,
        json_string(media_type.as_bytes()),
        json_string(digest.as_bytes())
    );
    if !annotations.is_empty() {
        let pairs: Vec<String> = annotations
            .iter()
            .map(|(k, v)| format!("{}:{}", json_string(k.as_bytes()), json_string(v.as_bytes())))
            .collect();
        out.push_str(&format!(r#","annotations":{{{}}}"#, pairs.join(",")));
    }
    if !platform.is_empty() {
        out.push_str(&format!(r#","platform":{platform}"#));
    }
    out.push('}');
    out
}

/// `ociReferenceName` (containerd archive/reference.go): a name's tag, or its digest.
fn ref_name(name: &Reference) -> String {
    name.tag
        .clone()
        .or_else(|| name.digest.as_ref().map(ToString::to_string))
        .unwrap_or_default()
}

/// What a blob of the layout holds.
enum Content {
    Dir,
    Bytes(Vec<u8>),
    File(std::path::PathBuf, u64),
}

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const LAYOUT: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

fn blob_name(d: &Digest) -> String {
    format!("blobs/{}/{}", d.algorithm().name(), d.hex())
}

/// The manifest an output names, and its media type: the store's, or with `docker` the
/// same in Docker's media types (a blob of its own).
fn manifest_for(made: &Made<'_>, docker: bool) -> (Vec<u8>, &'static str) {
    if docker {
        (
            export::docker_manifest(
                made.config,
                made.config_digest.to_string().as_bytes(),
                made.layers,
            ),
            "application/vnd.docker.distribution.manifest.v2+json",
        )
    } else {
        (
            made.manifest.to_vec(),
            "application/vnd.oci.image.manifest.v1+json",
        )
    }
}

/// The layout tar of the `oci` and `docker` outputs, as containerd's exporter writes it
/// for BuildKit: blobs, `index.json` with an entry per name (one without), `oci-layout`,
/// and for `docker` `manifest.json`; records in name order, every time 0, owner 0,
/// directories 0755, the documents 0644 and the blobs 0444. `docker` is Docker's media
/// types, `archive` the `docker` output.
///
/// An attested `docker` archive is the `oci` one with `manifest.json` naming the image
/// beside its index. buildx refuses one (build/opt.go), and BuildKit's exporter, given
/// no platform to pick from the index, writes no `manifest.json`, which `docker load`
/// without containerd's store cannot read; this one loads in both stores.
pub fn layout<W: Write>(
    store: &Store,
    made: &Made<'_>,
    docker: bool,
    archive: bool,
    names: &[Reference],
    created: &str,
    out: W,
) -> Result<(), String> {
    let (manifest, manifest_type) = manifest_for(made, docker);
    let manifest_digest = super::sha256(&manifest);
    let platform = platform(made.config)?;
    let mut records: BTreeMap<String, (u32, Content)> = BTreeMap::new();
    records.insert("blobs/".into(), (0o755, Content::Dir));
    records.insert("blobs/sha256/".into(), (0o755, Content::Dir));
    records.insert(
        blob_name(&manifest_digest),
        (0o444, Content::Bytes(manifest.clone())),
    );
    records.insert(
        blob_name(made.config_digest),
        (0o444, Content::Bytes(made.config.to_vec())),
    );
    let mut layer_names = Vec::new();
    for l in made.layers {
        let d = Digest::parse(&String::from_utf8_lossy(&l.digest)).map_err(|e| e.to_string())?;
        let path = store.blob_path(&d);
        let len = std::fs::metadata(&path).map_err(|e| format!("{d}: {e}"))?.len();
        records.insert(format!("blobs/{}/", d.algorithm().name()), (0o755, Content::Dir));
        layer_names.push(blob_name(&d));
        records.insert(blob_name(&d), (0o444, Content::File(path, len)));
    }
    // An attested image is named by its index, which has no platform of its own.
    let (named_type, named_digest, named_len, named_platform) =
        match if docker { None } else { attested(made, names)? } {
            Some(a) => {
                for (d, b) in a.blobs {
                    records.insert(blob_name(&d), (0o444, Content::Bytes(b)));
                }
                (OCI_INDEX, a.index.0.to_string(), a.index.1, String::new())
            }
            None => (
                manifest_type,
                manifest_digest.to_string(),
                manifest.len(),
                platform,
            ),
        };
    let mut entries = Vec::new();
    let mut base = made.descriptor_annotations.clone();
    base.insert(
        "org.opencontainers.image.created".to_string(),
        created.to_string(),
    );
    if names.is_empty() {
        entries.push(descriptor(
            named_type,
            &named_digest,
            named_len,
            &base,
            &named_platform,
        ));
    }
    for name in names {
        let mut a = base.clone();
        a.insert("io.containerd.image.name".into(), name.to_string());
        a.insert("org.opencontainers.image.ref.name".into(), ref_name(name));
        entries.push(descriptor(
            named_type,
            &named_digest,
            named_len,
            &a,
            &named_platform,
        ));
    }
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{}]}}"#,
        entries.join(",")
    );
    records.insert("index.json".into(), (0o644, Content::Bytes(index.into_bytes())));
    if archive {
        let tags = if names.is_empty() {
            "null".to_string()
        } else {
            let t: Vec<String> = names
                .iter()
                .map(|n| json_string(n.familiar().as_bytes()))
                .collect();
            format!("[{}]", t.join(","))
        };
        let layers: Vec<String> = layer_names.iter().map(|l| json_string(l.as_bytes())).collect();
        let doc = format!(
            r#"[{{"Config":{},"RepoTags":{tags},"Layers":[{}]}}]"#,
            json_string(blob_name(made.config_digest).as_bytes()),
            layers.join(",")
        );
        records.insert("manifest.json".into(), (0o644, Content::Bytes(doc.into_bytes())));
    }
    records.insert("oci-layout".into(), (0o444, Content::Bytes(LAYOUT.to_vec())));
    let mut tw = gotar::Writer::new(out);
    for (name, (mode, content)) in &records {
        let (typeflag, size) = match content {
            Content::Dir => (gotar::TYPE_DIR, 0),
            Content::Bytes(b) => (gotar::TYPE_REG, b.len() as u64),
            Content::File(_, len) => (gotar::TYPE_REG, *len),
        };
        tw.write_header(&Header {
            name: name.clone().into_bytes(),
            typeflag,
            mode: i64::from(*mode),
            size: i64::try_from(size).map_err(|e| e.to_string())?,
            mtime: gotar::Time::unix(0, 0),
            ..Header::default()
        })
        .map_err(|e| e.to_string())?;
        match content {
            Content::Dir => {}
            Content::Bytes(b) => tw.write_all(b).map_err(|e| e.to_string())?,
            Content::File(path, _) => {
                let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
                tw.copy_from(f).map_err(|e| e.to_string())?;
            }
        }
    }
    tw.finish()
        .map_err(|e| e.to_string())?
        .flush()
        .map_err(|e| e.to_string())
}

/// The `oci` output with `tar=false`: a content store in `dest` as buildkit's client
/// writes one (client/solve.go, client/ociindex): blobs it lacks added 0444, `ingest/`,
/// `oci-layout` 0644, and `index.json` read and written again under a lock, an entry per
/// name replacing one of the same name, `latest` its name without one.
pub fn layout_dir(
    store: &Store,
    made: &Made<'_>,
    names: &[Reference],
    created: &str,
    dest: &Path,
) -> Result<(), String> {
    let at = |p: &Path, e: io::Error| format!("{}: {e}", p.display());
    let blobs = dest.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).map_err(|e| at(&blobs, e))?;
    std::fs::create_dir_all(dest.join("ingest")).map_err(|e| at(dest, e))?;
    let (manifest, manifest_type) = manifest_for(made, false);
    let put = |d: &Digest, write: &mut dyn FnMut(&mut std::fs::File) -> io::Result<()>| {
        let path = dest.join(blob_name(d));
        if path.exists() {
            return Ok(());
        }
        let tmp = dest.join("ingest").join(d.hex());
        let mut f = std::fs::File::create(&tmp).map_err(|e| at(&tmp, e))?;
        write(&mut f).map_err(|e| at(&tmp, e))?;
        f.sync_all().map_err(|e| at(&tmp, e))?;
        set_mode(&tmp, 0o444).map_err(|e| at(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| at(&path, e))
    };
    for l in made.layers {
        let d = Digest::parse(&String::from_utf8_lossy(&l.digest)).map_err(|e| e.to_string())?;
        let from = store.blob_path(&d);
        put(&d, &mut |f| {
            io::copy(&mut std::fs::File::open(&from)?, f).map(|_| ())
        })?;
    }
    put(made.config_digest, &mut |f| f.write_all(made.config))?;
    put(made.manifest_digest, &mut |f| f.write_all(&manifest))?;
    let attestation = attested(made, names)?;
    if let Some(a) = &attestation {
        for (d, b) in &a.blobs {
            put(d, &mut |f| f.write_all(b))?;
        }
    }

    let named = match &attestation {
        Some(att) => serde_json::json!({
            "mediaType": OCI_INDEX,
            "digest": att.index.0.to_string(),
            "size": att.index.1,
        }),
        None => serde_json::json!({
            "mediaType": manifest_type,
            "digest": made.manifest_digest.to_string(),
            "size": manifest.len(),
            "platform": serde_json::from_str::<serde_json::Value>(&platform(made.config)?)
                .map_err(|e| e.to_string())?,
        }),
    };
    update_index(dest, &named, made.descriptor_annotations, names, created)
}

/// An annotation map as Go marshals a map[string]string: keys in order.
fn string_map(m: &serde_json::Map<String, serde_json::Value>) -> String {
    let sorted: BTreeMap<&String, &str> = m.iter().map(|(k, v)| (k, v.as_str().unwrap_or(""))).collect();
    let pairs: Vec<String> = sorted
        .iter()
        .map(|(k, v)| format!("{}:{}", json_string(k.as_bytes()), json_string(v.as_bytes())))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

/// A descriptor read from an index, written again as ocispec.Descriptor marshals.
fn go_descriptor(m: &serde_json::Value) -> Result<String, String> {
    let s = |k: &str| m.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
    let mut out = format!(
        r#"{{"mediaType":{},"digest":{},"size":{}"#,
        json_string(s("mediaType").as_bytes()),
        json_string(s("digest").as_bytes()),
        m.get("size").and_then(serde_json::Value::as_i64).unwrap_or(0)
    );
    if let Some(urls) = m
        .get("urls")
        .and_then(serde_json::Value::as_array)
        .filter(|u| !u.is_empty())
    {
        let u: Vec<String> = urls
            .iter()
            .map(|u| json_string(u.as_str().unwrap_or("").as_bytes()))
            .collect();
        out.push_str(&format!(r#","urls":[{}]"#, u.join(",")));
    }
    if let Some(a) = m
        .get("annotations")
        .and_then(serde_json::Value::as_object)
        .filter(|a| !a.is_empty())
    {
        out.push_str(&format!(r#","annotations":{}"#, string_map(a)));
    }
    if !s("data").is_empty() {
        out.push_str(&format!(r#","data":{}"#, json_string(s("data").as_bytes())));
    }
    if let Some(p) = m.get("platform").filter(|p| p.is_object()) {
        out.push_str(&format!(r#","platform":{}"#, platform_of(p)?));
    }
    if !s("artifactType").is_empty() {
        out.push_str(&format!(
            r#","artifactType":{}"#,
            json_string(s("artifactType").as_bytes())
        ));
    }
    out.push('}');
    Ok(out)
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(p: &Path, mode: u32) -> io::Result<()> {
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_readonly(mode & 0o200 == 0);
    std::fs::set_permissions(p, perm)
}

/// An image of several platforms (D77): the index that names them and their
/// attestations, its digest, and every blob it holds, here in the store.
pub struct Index<'a> {
    pub index: &'a [u8],
    pub digest: &'a Digest,
    pub blobs: &'a [Digest],
}

/// [`layout`]'s tar of an image of several platforms: its blobs from the store, the index
/// named by `index.json` (no platform of its own), records as [`layout`] writes them.
pub fn layout_index<W: Write>(
    store: &Store,
    image: &Index<'_>,
    names: &[Reference],
    created: &str,
    out: W,
) -> Result<(), String> {
    let mut records: BTreeMap<String, (u32, Content)> = BTreeMap::new();
    records.insert("blobs/".into(), (0o755, Content::Dir));
    records.insert("blobs/sha256/".into(), (0o755, Content::Dir));
    for d in image.blobs {
        let path = store.blob_path(d);
        let len = std::fs::metadata(&path).map_err(|e| format!("{d}: {e}"))?.len();
        records.insert(blob_name(d), (0o444, Content::File(path, len)));
    }
    records.insert(
        blob_name(image.digest),
        (0o444, Content::Bytes(image.index.to_vec())),
    );
    let base = BTreeMap::from([(
        "org.opencontainers.image.created".to_string(),
        created.to_string(),
    )]);
    let digest = image.digest.to_string();
    let mut entries = Vec::new();
    if names.is_empty() {
        entries.push(descriptor(OCI_INDEX, &digest, image.index.len(), &base, ""));
    }
    for name in names {
        let mut a = base.clone();
        a.insert("io.containerd.image.name".into(), name.to_string());
        a.insert("org.opencontainers.image.ref.name".into(), ref_name(name));
        entries.push(descriptor(OCI_INDEX, &digest, image.index.len(), &a, ""));
    }
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{}]}}"#,
        entries.join(",")
    );
    records.insert("index.json".into(), (0o644, Content::Bytes(index.into_bytes())));
    records.insert("oci-layout".into(), (0o444, Content::Bytes(LAYOUT.to_vec())));
    let mut tw = gotar::Writer::new(out);
    for (name, (mode, content)) in &records {
        let (typeflag, size) = match content {
            Content::Dir => (gotar::TYPE_DIR, 0),
            Content::Bytes(b) => (gotar::TYPE_REG, b.len() as u64),
            Content::File(_, len) => (gotar::TYPE_REG, *len),
        };
        tw.write_header(&Header {
            name: name.clone().into_bytes(),
            typeflag,
            mode: i64::from(*mode),
            size: i64::try_from(size).map_err(|e| e.to_string())?,
            mtime: gotar::Time::unix(0, 0),
            ..Header::default()
        })
        .map_err(|e| e.to_string())?;
        match content {
            Content::Dir => {}
            Content::Bytes(b) => tw.write_all(b).map_err(|e| e.to_string())?,
            Content::File(path, _) => {
                let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
                tw.copy_from(f).map_err(|e| e.to_string())?;
            }
        }
    }
    tw.finish()
        .map_err(|e| e.to_string())?
        .flush()
        .map_err(|e| e.to_string())
}

/// [`layout_dir`] of an image of several platforms: its blobs added, `index.json`'s entry
/// for each name the index.
pub fn layout_index_dir(
    store: &Store,
    image: &Index<'_>,
    names: &[Reference],
    created: &str,
    dest: &Path,
) -> Result<(), String> {
    let at = |p: &Path, e: io::Error| format!("{}: {e}", p.display());
    let blobs = dest.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).map_err(|e| at(&blobs, e))?;
    std::fs::create_dir_all(dest.join("ingest")).map_err(|e| at(dest, e))?;
    let put = |d: &Digest, write: &mut dyn FnMut(&mut std::fs::File) -> io::Result<()>| {
        let path = dest.join(blob_name(d));
        if path.exists() {
            return Ok(());
        }
        let tmp = dest.join("ingest").join(d.hex());
        let mut f = std::fs::File::create(&tmp).map_err(|e| at(&tmp, e))?;
        write(&mut f).map_err(|e| at(&tmp, e))?;
        f.sync_all().map_err(|e| at(&tmp, e))?;
        set_mode(&tmp, 0o444).map_err(|e| at(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| at(&path, e))
    };
    for d in image.blobs {
        let from = store.blob_path(d);
        put(d, &mut |f| {
            io::copy(&mut std::fs::File::open(&from)?, f).map(|_| ())
        })?;
    }
    put(image.digest, &mut |f| f.write_all(image.index))?;
    let named = serde_json::json!({
        "mediaType": OCI_INDEX,
        "digest": image.digest.to_string(),
        "size": image.index.len(),
    });
    update_index(dest, &named, &BTreeMap::new(), names, created)
}

/// The `index.json` of the layout in `dest`, read and written again under a lock as
/// buildkit's client writes it (client/ociindex): an entry per name of `named`, the
/// descriptor's annotations `base`, `created` and the name, replacing one of the same
/// name; `latest` its name without one.
fn update_index(
    dest: &Path,
    named: &serde_json::Value,
    base: &BTreeMap<String, String>,
    names: &[Reference],
    created: &str,
) -> Result<(), String> {
    let at = |p: &Path, e: io::Error| format!("{}: {e}", p.display());
    let index_path = dest.join("index.json");
    let lock_path = dest.join("index.json.lock");
    let lock = std::fs::File::create(&lock_path).map_err(|e| at(&lock_path, e))?;
    if lock.try_lock().is_err() {
        return Err(format!("could not lock {}", lock_path.display()));
    }
    let written = (|| {
        std::fs::write(dest.join("oci-layout"), LAYOUT).map_err(|e| at(dest, e))?;
        set_mode(&dest.join("oci-layout"), 0o644).map_err(|e| at(dest, e))?;
        let old = match std::fs::read(&index_path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(at(&index_path, e)),
        };
        let mut index: serde_json::Value = if old.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&old).map_err(|e| {
                format!(
                    "could not unmarshal {} ({}): {e}",
                    index_path.display(),
                    json_string(&old)
                )
            })?
        };
        let mut manifests: Vec<serde_json::Value> = index
            .get_mut("manifests")
            .and_then(|m| m.as_array_mut())
            .map(std::mem::take)
            .unwrap_or_default();
        let mut base: serde_json::Map<String, serde_json::Value> =
            base.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect();
        base.insert("org.opencontainers.image.created".into(), created.into());
        let tagged: Vec<(String, String)> = if names.is_empty() {
            vec![(String::new(), "latest".into())]
        } else {
            names.iter().map(|n| (n.to_string(), ref_name(n))).collect()
        };
        for (image, reference) in tagged {
            let mut a = base.clone();
            if !image.is_empty() {
                a.insert("io.containerd.image.name".into(), image.clone().into());
            }
            a.insert(
                "org.opencontainers.image.ref.name".into(),
                reference.clone().into(),
            );
            let annotation = |m: &serde_json::Value, k: &str| {
                m.get("annotations")
                    .and_then(|a| a.get(k))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            manifests.retain(|m| {
                annotation(m, "org.opencontainers.image.ref.name") != reference
                    || annotation(m, "io.containerd.image.name") != image
            });
            let mut entry = named.clone();
            if let Some(fields) = entry.as_object_mut() {
                fields.insert("annotations".into(), serde_json::Value::Object(a));
            }
            manifests.push(entry);
        }
        let schema = index
            .get("schemaVersion")
            .and_then(serde_json::Value::as_i64)
            .filter(|&v| v != 0)
            .unwrap_or(2);
        let media = index
            .get("mediaType")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(OCI_INDEX)
            .to_string();
        let mut doc = format!(
            r#"{{"schemaVersion":{schema},"mediaType":{}"#,
            json_string(media.as_bytes())
        );
        if let Some(t) = index
            .get("artifactType")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
        {
            doc.push_str(&format!(r#","artifactType":{}"#, json_string(t.as_bytes())));
        }
        let entries: Vec<String> = manifests.iter().map(go_descriptor).collect::<Result<_, _>>()?;
        doc.push_str(&format!(r#","manifests":[{}]"#, entries.join(",")));
        if let Some(a) = index
            .get("annotations")
            .and_then(serde_json::Value::as_object)
            .filter(|a| !a.is_empty())
        {
            doc.push_str(&format!(r#","annotations":{}"#, string_map(a)));
        }
        doc.push('}');
        std::fs::write(&index_path, doc).map_err(|e| at(&index_path, e))
    })();
    let _ = lock.unlock();
    let _ = std::fs::remove_file(&lock_path);
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_output_takes_its_own_epoch_over_the_build_s() {
        let attrs = |v: &str| BTreeMap::from([("source-date-epoch".to_string(), v.to_string())]);
        assert_eq!(epoch(&BTreeMap::new(), Some(7)), Ok(Some(7)));
        assert_eq!(epoch(&attrs("9"), Some(7)), Ok(Some(9)));
        assert_eq!(epoch(&attrs(""), Some(7)), Ok(None));
        assert_eq!(
            epoch(&attrs("x"), None),
            Err(r#"invalid source-date-epoch: strconv.ParseInt: parsing "x": invalid syntax: strconv.ParseInt: parsing "x": invalid syntax"#.into())
        );
    }

    #[test]
    fn platforms_marshal_as_ocispec_does() {
        let v = serde_json::json!({"architecture":"arm64","os":"linux","variant":"v8"});
        assert_eq!(
            platform_of(&v).unwrap(),
            r#"{"architecture":"arm64","os":"linux","variant":"v8"}"#
        );
    }
}
