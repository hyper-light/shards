//! Builds for several platforms at once (D77), as BuildKit's frontend builds each of
//! `--platform`'s and its exporter writes one image of them: each platform built by the
//! build itself ([`super::run`]), as one of several (its steps named with its platform),
//! into a layout of its own, its local outputs in a directory of its platform's name; then
//! the index of their manifests and attestations (platforms first, as Docker's), which the
//! image's names resolve to, its ID, what it pushes and what its OCI outputs hold.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use shards_cmdline::buildflags;
use shards_cmdline::flags::Parsed;
use shards_dockerfile::platform::{self, Platform};
use shards_image::oci::{self, Descriptor};
use shards_image::reference::{Digest, Reference};
use shards_image::store::{self, Store};

use super::{Progress, Provenance, output, provenance, show};

thread_local! {
    /// The platform's build of several that runs on this thread, if one does.
    static SUB: RefCell<Option<Sub>> = const { RefCell::new(None) };
}

/// What one platform's build of several is given and gives back.
#[derive(Debug, Default)]
pub(super) struct Sub {
    /// The progress display's next vertex: the platforms' builds number on.
    pub next: usize,
    /// buildx's provenance of the platform's build (v0.2), for the metadata file.
    pub provenance: Option<provenance::Json>,
    /// Whether it is the first platform's, which says what the builds share.
    pub first: bool,
    /// What base images resolved to, by name and platform: one resolve of each for the
    /// builds of them all, as BuildKit's (one vertex for an identifier and platform).
    pub answered: super::Answered,
    /// The bases those resolved, by what the planner names them (`Bases::resolved`).
    pub bases: BTreeMap<String, super::Base>,
    /// The build cache's records the platform's build used, and its image's layers: what
    /// `--cache-to` exports of the builds of them all (D87).
    pub keys: Vec<String>,
    pub image_layers: std::collections::BTreeSet<String>,
}

/// Whether this build is one platform's of several.
pub(super) fn active() -> bool {
    SUB.with(|s| s.borrow().is_some())
}

/// `f` of the platform's build of several that runs, if one does.
pub(super) fn with<T>(f: impl FnOnce(&mut Sub) -> T) -> Option<T> {
    SUB.with(|s| s.borrow_mut().as_mut().map(f))
}

/// A platform's directory name in a local output split by platform (BuildKit's
/// `platforms.Format`, `/` as `_`): `linux_amd64`, `linux_arm_v7`.
pub(crate) fn dir_name(p: &Platform) -> String {
    show(&platform::format(p)).replace('/', "_")
}

/// Where a tar output's part for platform `p` is written in `stage`.
fn tar_part(stage: &Path, output: usize, p: &Platform) -> PathBuf {
    stage.join(format!("tar-{output}-{}.tar", dir_name(p)))
}

/// A tar output of several platforms, as BuildKit's tar exporter writes one (its file
/// system split by platform): for each platform, in name order, a directory of its name
/// (0755, owner 0, time 0), then each entry of its own tar within it, hardlinks' targets
/// too, each header written as Go's archive/tar writes a new one (D86).
fn join_tars(parts: &[(String, PathBuf)], out: impl std::io::Write) -> Result<(), String> {
    use shards_archive::tar::{self as gotar, Header, TYPE_DIR, TYPE_LINK};
    let err = |e: shards_archive::Error| e.to_string();
    let mut w = gotar::Writer::new(out);
    for (dir, path) in parts {
        w.write_header(&Header {
            typeflag: TYPE_DIR,
            name: format!("{dir}/").into_bytes(),
            mode: 0o755,
            mtime: gotar::Time::unix(0, 0),
            ..Header::default()
        })
        .map_err(err)?;
        let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut r = gotar::Reader::new(std::io::BufReader::new(f));
        let within = |name: &[u8]| [format!("{dir}/").as_bytes(), name].concat();
        while let Some(mut h) = r.next_header().map_err(err)? {
            h.name = within(&h.name);
            if h.typeflag == TYPE_LINK {
                h.linkname = within(&h.linkname);
            }
            h.format = gotar::Format::UNKNOWN;
            w.write_header(&h).map_err(err)?;
            w.copy_from(&mut r).map_err(err)?;
        }
    }
    let mut out = w.finish().map_err(err)?;
    out.flush().map_err(|e| e.to_string())
}

/// One field of a CSV record as Go's encoding/csv writes it.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// One platform's image, as its build left it in its layout.
struct Built {
    platform: Platform,
    manifest: Descriptor,
    attestation: Option<Descriptor>,
    /// Every blob the manifest and its attestation name, themselves too.
    blobs: Vec<Digest>,
    attestation_blobs: Vec<Digest>,
    provenance: Option<provenance::Json>,
}

/// What a platform's layout holds: its manifest, its attestation where it has one, and
/// the blobs each names.
struct Layout {
    manifest: Descriptor,
    attestation: Option<Descriptor>,
    blobs: Vec<Digest>,
    attestation_blobs: Vec<Digest>,
}

/// The layout in `dir`, its index's first image: the platform's manifest, and its
/// attestation where the index has one; every blob taken into the store.
fn read_layout(store: &Store, dir: &Path) -> Result<Layout, String> {
    let at = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let blob_path = |d: &Descriptor| -> Result<PathBuf, String> {
        let digest = d.digest().map_err(|e| e.to_string())?;
        Ok(dir
            .join("blobs")
            .join(digest.algorithm().name())
            .join(digest.hex()))
    };
    // Each blob the layout holds, into the store unless it is there.
    let take = |d: &Descriptor| -> Result<Digest, String> {
        let digest = d.digest().map_err(|e| e.to_string())?;
        if !store.has(&digest) {
            let path = blob_path(d)?;
            let mut f = std::fs::File::open(&path).map_err(|e| at(&path, e))?;
            let size = u64::try_from(d.size).map_err(|e| e.to_string())?;
            store.ingest(&digest, size, &mut f).map_err(|e| e.to_string())?;
        }
        Ok(digest)
    };
    let read = |d: &Descriptor| -> Result<Vec<u8>, String> {
        let path = blob_path(d)?;
        std::fs::read(&path).map_err(|e| at(&path, e))
    };
    let top = dir.join("index.json");
    let index: oci::Index =
        serde_json::from_slice(&std::fs::read(&top).map_err(|e| at(&top, e))?).map_err(|e| e.to_string())?;
    let first = index
        .manifests
        .first()
        .cloned()
        .ok_or_else(|| format!("{}: names nothing", top.display()))?;
    let (manifest, attestation) = if first.media_type == oci::media::OCI_INDEX {
        let inner: oci::Index = serde_json::from_slice(&read(&first)?).map_err(|e| e.to_string())?;
        let attested = |d: &&Descriptor| d.platform.as_ref().is_some_and(|p| p.os == "unknown");
        let manifest = inner
            .manifests
            .iter()
            .find(|d| !attested(d))
            .cloned()
            .ok_or("an attested index without its image")?;
        (manifest, inner.manifests.iter().find(attested).cloned())
    } else {
        (first, None)
    };
    let mut blobs = Vec::new();
    let mut attestation_blobs = Vec::new();
    for (d, into) in std::iter::once((&manifest, &mut blobs))
        .chain(attestation.as_ref().map(|a| (a, &mut attestation_blobs)))
    {
        let doc = read(d)?;
        let oci::Document::Manifest(m) =
            oci::parse_document(&doc, &d.media_type).map_err(|e| e.to_string())?
        else {
            return Err(format!("{}: an index where a manifest was", d.digest));
        };
        into.push(take(d)?);
        for part in std::iter::once(&m.config).chain(&m.layers) {
            into.push(take(part)?);
        }
    }
    Ok(Layout {
        manifest,
        attestation,
        blobs,
        attestation_blobs,
    })
}

/// The build of `parsed` for each of `platforms`, and the one image of them its outputs
/// take (see the module's documentation). `outputs` are the build's, read; local outputs
/// are written by each platform's build, every other here.
#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    parsed: &Parsed,
    status: &Cell<u8>,
    outputs: &[buildflags::Output],
    platforms: &[Platform],
    progress: &RefCell<Progress>,
    provenance_asked: &Provenance,
    descriptor_annotations: &BTreeMap<String, String>,
    pushes: bool,
) -> Result<(), String> {
    for o in outputs {
        // BuildKit's own refusal (v0.28.1 exporter/oci): a docker archive holds one image.
        if o.kind == "docker" && !matches!(o.dest, buildflags::Dest::Store) {
            return Err(
                "failed to build: failed to solve: docker exporter does not currently support exporting manifest lists"
                    .into(),
            );
        }
        let why = match (o.kind.as_str(), o.attrs.get("platform-split").map(String::as_str)) {
            ("local", Some(v)) if shards_cmdline::go::parse_bool(v) == Ok(false) => {
                "a local output not split by platform"
            }
            _ => continue,
        };
        return Err(format!(
            "{why} of several platforms is not supported by shards yet: build each platform for it"
        ));
    }
    // `--cache-to`: an inline cache in each platform's image, as its own config carries
    // it; every other written once, of the records of every platform's build (D87).
    let env = |k: &str| std::env::var(k).ok();
    let mut inline_raw = Vec::new();
    let mut exported = Vec::new();
    for raw in parsed.many("cache-to") {
        for e in buildflags::cache_entries(std::slice::from_ref(raw), &env)? {
            if e.kind == "inline" {
                inline_raw.push(raw.clone());
            } else {
                exported.push(e);
            }
        }
    }
    let mut keys: Vec<String> = Vec::new();
    let mut image_layers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let home = shards_ipc::home()?;
    let store = crate::pull::store(&home)?;
    let _lease = store.lease().map_err(|e| e.to_string())?;
    let stage = store.stage().map_err(|e| e.to_string())?;
    let tags = super::canonical_names(parsed.many("tag"))?;
    // The layers each platform's image takes: the image outputs' compression and rewrite.
    let image_attrs: String = outputs
        .iter()
        .find(|o| matches!(o.kind.as_str(), "image" | "moby" | "oci"))
        .map(|o| {
            o.attrs
                .iter()
                .filter(|(k, _)| {
                    matches!(
                        k.as_str(),
                        "compression"
                            | "compression-level"
                            | "force-compression"
                            | "rewrite-timestamp"
                            | "source-date-epoch"
                    )
                })
                .map(|(k, v)| format!(",{}", csv_field(&format!("{k}={v}"))))
                .collect()
        })
        .unwrap_or_default();
    let mut built = Vec::new();
    let mut answered = BTreeMap::new();
    let mut bases = BTreeMap::new();
    for (n, p) in platforms.iter().enumerate() {
        let layout = stage.path().join(dir_name(p));
        let mut outs = vec![format!(
            "type=oci,{},tar=false{image_attrs}{}",
            csv_field(&format!("dest={}", layout.display())),
            if tags.is_empty() {
                String::new()
            } else {
                format!(",{}", csv_field(&format!("name={}", tags.join(","))))
            }
        )];
        // Each tar output's part: the platform's own tar, joined after (D86).
        for (i, _) in outputs.iter().enumerate().filter(|(_, o)| o.kind == "tar") {
            outs.push(format!(
                "type=tar,{}",
                csv_field(&format!("dest={}", tar_part(stage.path(), i, p).display()))
            ));
        }
        for o in outputs.iter().filter(|o| o.kind == "local") {
            let buildflags::Dest::Dir(dest) = &o.dest else {
                continue;
            };
            outs.push(format!(
                "type=local,{}",
                csv_field(&format!("dest={}", dest.join(dir_name(p)).display()))
            ));
        }
        let sub = parsed
            .with_many("platform", vec![show(&platform::format(p))])
            .with_many("output", outs)
            .with_bool("push", false)
            .with_bool("load", false)
            .with_many("tag", Vec::new())
            .with_many("cache-to", inline_raw.clone())
            .with_text("metadata-file", "")
            .with_text("iidfile", "");
        SUB.with(|s| {
            *s.borrow_mut() = Some(Sub {
                next: progress.borrow().next,
                provenance: None,
                first: n == 0,
                answered: std::mem::take(&mut answered),
                bases: std::mem::take(&mut bases),
                ..Sub::default()
            })
        });
        let r = super::run(&sub, status);
        let given = SUB.with(|s| s.borrow_mut().take()).unwrap_or_default();
        progress.borrow_mut().next = given.next;
        answered = given.answered;
        bases = given.bases;
        for k in given.keys {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        image_layers.extend(given.image_layers);
        r?;
        let held = read_layout(&store, &layout)?;
        built.push(Built {
            platform: p.clone(),
            manifest: held.manifest,
            attestation: held.attestation,
            blobs: held.blobs,
            attestation_blobs: held.attestation_blobs,
            provenance: given.provenance,
        });
    }
    // The external caches, once, of every platform's records.
    super::remote::export(&exported, &keys, &store, &image_layers, progress, &env)?;
    // Each tar output: its platforms' tars, each in a directory of its platform's name.
    for (i, o) in outputs.iter().enumerate().filter(|(_, o)| o.kind == "tar") {
        let mut parts: Vec<(String, PathBuf)> = platforms
            .iter()
            .map(|p| (dir_name(p), tar_part(stage.path(), i, p)))
            .collect();
        parts.sort();
        match &o.dest {
            buildflags::Dest::Stdout => join_tars(&parts, std::io::BufWriter::new(std::io::stdout().lock()))?,
            buildflags::Dest::File(path) => join_tars(
                &parts,
                std::io::BufWriter::new(
                    std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?,
                ),
            )?,
            _ => {}
        }
    }
    // The index: the platforms' manifests, then their attestations where the build
    // attests (D71: by default, unless BUILDX_NO_DEFAULT_ATTESTATIONS; Docker attests every
    // image of several platforms, its OCI outputs' too).
    let attest = match provenance_asked {
        Provenance::Default => super::attests(&|k| std::env::var(k).ok())?,
        Provenance::Off => false,
        Provenance::Explicit { .. } => true,
    };
    let parts: Vec<(String, String, String)> = built
        .iter()
        .map(|b| {
            let variant = show(&b.platform.variant);
            let arch = show(&b.platform.architecture);
            // linux/arm64/v8 is linux/arm64 in a descriptor (platforms.Normalize).
            let variant = if arch == "arm64" && variant == "v8" {
                String::new()
            } else {
                variant
            };
            (arch, show(&b.platform.os), variant)
        })
        .collect();
    let manifests: Vec<(String, usize)> = built
        .iter()
        .map(|b| {
            (
                b.manifest.digest.clone(),
                usize::try_from(b.manifest.size).unwrap_or(0),
            )
        })
        .collect();
    let images: Vec<(&str, usize, (&str, &str, &str))> = manifests
        .iter()
        .zip(&parts)
        .map(|((d, n), (a, o, v))| (d.as_str(), *n, (a.as_str(), o.as_str(), v.as_str())))
        .collect();
    let atts: Vec<(&str, usize, &str)> = if attest {
        built
            .iter()
            .filter_map(|b| {
                let a = b.attestation.as_ref()?;
                Some((
                    a.digest.as_str(),
                    usize::try_from(a.size).unwrap_or(0),
                    b.manifest.digest.as_str(),
                ))
            })
            .collect()
    } else {
        Vec::new()
    };
    let index = provenance::index_of(&images, &atts);
    let index_digest = super::sha256(index.as_bytes());
    store
        .ingest(&index_digest, index.len() as u64, &mut index.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut blobs: Vec<Digest> = Vec::new();
    for b in &built {
        blobs.extend(b.blobs.iter().cloned());
        if attest {
            blobs.extend(b.attestation_blobs.iter().cloned());
        }
    }
    let mut contents = vec![index_digest.clone()];
    contents.extend(blobs.iter().cloned());

    let kept = outputs.is_empty()
        || outputs.iter().any(|o| {
            matches!(o.dest, buildflags::Dest::Store) && matches!(o.kind.as_str(), "image" | "moby")
        });
    if kept || pushes {
        let v = progress.borrow_mut().start("exporting to image");
        progress.borrow().line(&v, "exporting layers done");
        progress
            .borrow()
            .line(&v, &format!("exporting manifest list {index_digest} done"));
        // What a run of it takes: this host's platform's manifest, else the first.
        let host = super::host_platform();
        let runs = built
            .iter()
            .find(|b| platform::format_all(&b.platform) == platform::format_all(&host))
            .or_else(|| built.first())
            .map(|b| b.manifest.clone())
            .ok_or("no platform was built")?;
        let runs = Descriptor {
            platform: None,
            annotations: Default::default(),
            ..runs
        };
        if kept {
            for tag in parsed.many("tag") {
                let reference = Reference::parse(tag).map_err(|e| format!("invalid tag {tag:?}: {e}"))?;
                store
                    .tag(&reference.to_string(), &runs, &index_digest, &contents)
                    .map_err(|e| e.to_string())?;
                progress.borrow().line(&v, &format!("naming to {reference} done"));
            }
            if parsed.many("tag").is_empty() {
                let dangling = format!("{}{index_digest}", store::DANGLING);
                store
                    .tag(&dangling, &runs, &index_digest, &contents)
                    .map_err(|e| e.to_string())?;
            }
        }
        if pushes {
            let pushed = Descriptor {
                media_type: oci::media::OCI_INDEX.into(),
                digest: index_digest.to_string(),
                size: i64::try_from(index.len()).map_err(|e| e.to_string())?,
                platform: None,
                annotations: Default::default(),
            };
            for tag in parsed.many("tag") {
                let reference = Reference::parse(tag).map_err(|e| format!("invalid tag {tag:?}: {e}"))?;
                progress.borrow().line(&v, "pushing layers");
                let registry =
                    crate::pull::registry_for_push(&reference, None, None, &|k| std::env::var(k).ok())?;
                shards_registry::push::push(
                    &registry,
                    &store,
                    &pushed,
                    reference.tag.as_deref(),
                    None,
                    &|_, _| {},
                )
                .map_err(|e| super::fail_export(progress, &v, &e.to_string()))?;
                progress.borrow().line(
                    &v,
                    &format!("pushing manifest for {reference}@{index_digest} done"),
                );
            }
        }
        progress.borrow().done(&v);
    }
    // The OCI outputs: the index and every blob it names.
    let image = output::Index {
        index: index.as_bytes(),
        digest: &index_digest,
        blobs: &blobs,
    };
    for o in outputs.iter().filter(|o| o.kind == "oci") {
        if matches!(o.dest, buildflags::Dest::Store) {
            continue;
        }
        let v = progress.borrow_mut().start("exporting to oci image format");
        let fail = |e: String| super::fail_export(progress, &v, &e);
        let names: Vec<Reference> = match o.attrs.get("name") {
            Some(n) => n
                .split(',')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect(),
            None => parsed.many("tag").to_vec(),
        }
        .iter()
        .map(|n| Reference::parse(n.trim()).map_err(|e| fail(e.to_string())))
        .collect::<Result<_, _>>()?;
        let epoch = output::epoch(&o.attrs, None).map_err(&fail)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let created = shards_dockerfile::go::Time::from_unix(epoch.unwrap_or(now))
            .rfc3339_nano()
            .map_err(|e| fail(show(&e)))?;
        progress
            .borrow()
            .line(&v, &format!("exporting manifest list {index_digest} done"));
        match &o.dest {
            buildflags::Dest::Dir(dest) => {
                output::layout_index_dir(&store, &image, &names, &created, dest).map_err(fail)?;
            }
            buildflags::Dest::Stdout => {
                let out = std::io::BufWriter::new(std::io::stdout().lock());
                output::layout_index(&store, &image, &names, &created, out).map_err(fail)?;
                progress.borrow().line(&v, "sending tarball done");
            }
            buildflags::Dest::File(path) => {
                let f = std::fs::File::create(path).map_err(|e| fail(format!("{}: {e}", path.display())))?;
                output::layout_index(&store, &image, &names, &created, std::io::BufWriter::new(f))
                    .map_err(fail)?;
                progress.borrow().line(&v, "sending tarball done");
            }
            buildflags::Dest::Store => {}
        }
        progress.borrow().done(&v);
    }
    // The metadata file: the index, and each platform's provenance.
    let shown: Vec<String> = built
        .iter()
        .map(|b| show(&platform::format(&b.platform)))
        .collect();
    let infos: Vec<(Option<&str>, &provenance::Json)> = built
        .iter()
        .zip(&shown)
        .filter_map(|(b, s)| Some((Some(s.as_str()), b.provenance.as_ref()?)))
        .collect();
    let imaged = kept || pushes || outputs.iter().any(|o| o.kind == "oci");
    super::write_metadata(
        parsed,
        &super::metadata(
            &super::build_ref()?,
            imaged.then_some((&index_digest, index.len(), tags.as_slice())),
            descriptor_annotations,
            &infos,
            oci::media::OCI_INDEX,
        ),
    )?;
    let _ = std::io::stdout().flush();
    super::finish(parsed, &index_digest.to_string())
}
