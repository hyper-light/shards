//! `shards build agent`, and agents, harnesses and MCP servers as OSI artifacts (D54;
//! AGENTFILE_ARCH.md §8 Q1, §12.17): made from a directory and its config, kept in the
//! image store under their names, pushed to and pulled from OCI registries as images
//! are, listed, inspected and removed.
//!
//! An artifact's content holds files, directories and links alone: no device nodes,
//! FIFOs, sockets or set-ID bits, no symlink whose target leaves the directory, no
//! whiteouts (§9.2). It is checked when made, when pulled, and when a build takes it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest as _, Sha256};
use shards_image::oci::{self, Descriptor, Document};
use shards_image::osi::{self, Config, Kind};
use shards_image::reference::{Digest, Reference};
use shards_image::store::Store;

/// The file a directory's config is read from, by kind.
fn config_file(kind: Kind) -> &'static str {
    match kind {
        Kind::Agent => "agent.json",
        Kind::Harness => "harness.json",
        Kind::Mcp => "mcp.json",
    }
}

fn kind_of(word: &str) -> Option<Kind> {
    match word {
        "agent" | "agents" => Some(Kind::Agent),
        "harness" | "harnesses" => Some(Kind::Harness),
        "mcp" | "mcps" => Some(Kind::Mcp),
        _ => None,
    }
}

/// `shards agent ACTION [ARG...]` (and `harness`, `mcp`), as the grammar's `shards ACTION
/// agent ...` says it.
pub fn command(word: &str, args: impl Iterator<Item = OsString>) -> ExitCode {
    let Some(kind) = kind_of(word) else {
        return fail(&format!("unknown kind {word:?}"));
    };
    let args: Result<Vec<String>, String> = args
        .map(|a| {
            a.into_string()
                .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
        })
        .collect();
    let result = args.and_then(|args| {
        let (action, rest) = args.split_first().ok_or_else(|| usage(kind))?;
        match action.as_str() {
            "build" => build(kind, rest),
            "push" => push(kind, rest),
            "pull" => pull_cmd(kind, rest),
            "ls" | "list" => list(kind),
            "inspect" => inspect(kind, rest),
            "rm" | "remove" | "delete" => remove(kind, rest),
            "-h" | "--help" | "help" => {
                let _ = writeln!(io::stdout(), "{}", usage(kind));
                Ok(())
            }
            other => Err(format!("unknown action {other:?}\n{}", usage(kind))),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// An OSI artifact of `kind`, with its article.
fn a(kind: Kind) -> &'static str {
    match kind {
        Kind::Agent => "an OSI agent",
        Kind::Harness => "an OSI harness",
        Kind::Mcp => "an OSI MCP server",
    }
}

fn fail(e: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "ERROR: {e}");
    ExitCode::FAILURE
}

fn usage(kind: Kind) -> String {
    let w = match kind {
        Kind::Agent => "agent",
        Kind::Harness => "harness",
        Kind::Mcp => "mcp",
    };
    format!(
        "Usage:\n  shards build {w} DIR -t NAME [--platform LIST]   make one of DIR and its {}\n  shards push {w} NAME\n  shards pull {w} NAME\n  shards ls {w}\n  shards inspect {w} NAME\n  shards rm {w} NAME",
        config_file(kind)
    )
}

fn sha256(bytes: &[u8]) -> Result<Digest, String> {
    let hex: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    Digest::parse(&format!("sha256:{hex}")).map_err(|e| e.to_string())
}

fn store() -> Result<Store, String> {
    crate::pull::store(&shards_ipc::home()?)
}

/// The name an artifact goes by: as `docker pull` reads one.
fn reference(name: &str) -> Result<Reference, String> {
    Reference::parse(name).map_err(|e| format!("invalid reference {name:?}: {e}"))
}

/// `shards build agent DIR -t NAME`.
fn build(kind: Kind, args: &[String]) -> Result<(), String> {
    let (mut dir, mut name) = (None, None);
    let mut platforms: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-t" | "--tag" => name = Some(it.next().ok_or("-t needs a name")?.clone()),
            s if s.starts_with("--tag=") => name = Some(s.trim_start_matches("--tag=").to_string()),
            "--platform" => platforms.push(it.next().ok_or("--platform needs platforms")?.clone()),
            s if s.starts_with("--platform=") => {
                platforms.push(s.trim_start_matches("--platform=").to_string())
            }
            s if s.starts_with('-') => return Err(format!("unknown flag {s}\n{}", usage(kind))),
            s if dir.is_none() => dir = Some(PathBuf::from(s)),
            s => return Err(format!("one directory only, not {s:?} too")),
        }
    }
    let dir = dir.ok_or_else(|| usage(kind))?;
    let name = reference(&name.ok_or("-t names what it is called")?)?;
    let store = store()?;
    let _lease = store.lease().map_err(|e| e.to_string())?;
    if !platforms.is_empty() {
        return build_index(kind, &dir, &name, &platforms, &store);
    }
    let (desc, _) = make(kind, &dir, &store)?;
    let digest = desc.digest().map_err(|e| e.to_string())?;
    store
        .tag(&name.to_string(), &desc, &digest, &contents(&store, &desc)?)
        .map_err(|e| e.to_string())?;
    let _ = writeln!(io::stdout(), "{digest}");
    Ok(())
}

/// An artifact of several platforms (§8 Q1): an index of a manifest for each, its config
/// naming its platform, its content `DIR/<os>_<arch>[_<variant>]` where there is one, as
/// BuildKit's local exporter lays out several platforms, else `DIR` itself, those
/// directories left out. The name resolves to the index, and is this host's manifest.
fn build_index(
    kind: Kind,
    dir: &Path,
    name: &Reference,
    given: &[String],
    store: &Store,
) -> Result<(), String> {
    let host = crate::build::host_platform();
    let wanted = crate::build::target_platforms(given, &host)?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let subdirs: Vec<String> = wanted.iter().map(crate::build::multi::dir_name).collect();
    let skip: Vec<&str> = subdirs.iter().map(String::as_str).collect();
    let mut manifests = Vec::new();
    let mut all = Vec::new();
    for (p, sub) in wanted.iter().zip(&subdirs) {
        let content = dir.join(sub);
        let platform = osi::Platform {
            os: text(&p.os),
            architecture: text(&p.architecture),
            variant: (!p.variant.is_empty()).then(|| text(&p.variant)),
        };
        let (mut desc, _) = if content.is_dir() {
            make_in(kind, dir, &content, &[], store, Some(&platform))?
        } else {
            make_in(kind, dir, dir, &skip, store, Some(&platform))?
        };
        all.extend(contents(store, &desc)?);
        desc.platform = Some(oci::Platform {
            architecture: platform.architecture.clone(),
            os: platform.os.clone(),
            variant: platform.variant.clone(),
            os_features: Vec::new(),
        });
        manifests.push(desc);
    }
    let entries = manifests
        .iter()
        .map(|d| {
            let mut v = serde_json::to_value(d).map_err(|e| e.to_string())?;
            if let Some(o) = v.as_object_mut() {
                o.insert("artifactType".into(), kind.artifact_type().into());
            }
            Ok(v)
        })
        .collect::<Result<Vec<serde_json::Value>, String>>()?;
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": oci::media::OCI_INDEX,
        "artifactType": kind.artifact_type(),
        "manifests": entries,
    }))
    .map_err(|e| e.to_string())?;
    let index_digest = sha256(&index)?;
    store
        .ingest(&index_digest, index.len() as u64, &mut &index[..])
        .map_err(|e| e.to_string())?;
    all.insert(0, index_digest.clone());
    let parsed = oci::Index {
        schema_version: 2,
        media_type: Some(oci::media::OCI_INDEX.into()),
        manifests: manifests.clone(),
    };
    // This host's manifest, else the first: what the name gives a run here.
    let ours = shards_image::platform::select(&parsed, &shards_image::platform::guest())
        .or(manifests.first())
        .cloned()
        .ok_or("no platforms")?;
    let ours = Descriptor {
        platform: None,
        ..ours
    };
    store
        .tag(&name.to_string(), &ours, &index_digest, &all)
        .map_err(|e| e.to_string())?;
    let _ = writeln!(io::stdout(), "{index_digest}");
    Ok(())
}

/// The artifact of `dir`: its config (its `agent.json`, with `schemaVersion` filled in),
/// and its files in one uncompressed tar, all stored. Returns its manifest's descriptor.
pub fn make(kind: Kind, dir: &Path, store: &Store) -> Result<(Descriptor, Config), String> {
    make_in(kind, dir, dir, &[], store, None)
}

/// [`make`] of the config in `dir` and the content of `content`, `skip` left out of it,
/// for `platform` where one is given: its config names it, and refuses another.
fn make_in(
    kind: Kind,
    dir: &Path,
    content: &Path,
    skip: &[&str],
    store: &Store,
    platform: Option<&osi::Platform>,
) -> Result<(Descriptor, Config), String> {
    let cfg_path = dir.join(config_file(kind));
    let text = fs::read(&cfg_path).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    let mut v: serde_json::Value =
        serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    if let Some(o) = v.as_object_mut() {
        o.entry("schemaVersion").or_insert(osi::SCHEMA_VERSION.into());
    }
    let bytes = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
    let mut config = Config::parse(&bytes).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    if let Some(p) = platform {
        if let Some(said) = config.platform.as_ref().filter(|said| *said != p) {
            return Err(format!(
                "{} says its platform is {}/{}, not {}/{}",
                cfg_path.display(),
                said.os,
                said.architecture,
                p.os,
                p.architecture
            ));
        }
        config.platform = Some(p.clone());
    }
    let config_bytes = config.to_bytes().map_err(|e| e.to_string())?;
    let mut tar = Vec::new();
    let mut skipped = vec![config_file(kind)];
    skipped.extend_from_slice(skip);
    pack(content, &skipped, &mut tar)?;
    let put = |bytes: &[u8]| -> Result<Digest, String> {
        let d = sha256(bytes)?;
        store
            .ingest(&d, bytes.len() as u64, &mut &bytes[..])
            .map_err(|e| e.to_string())?;
        Ok(d)
    };
    let desc = |media: &str, d: &Digest, size: usize| -> Result<Descriptor, String> {
        Ok(Descriptor {
            media_type: media.to_string(),
            digest: d.to_string(),
            size: i64::try_from(size).map_err(|e| e.to_string())?,
            platform: None,
            annotations: BTreeMap::new(),
        })
    };
    let config_desc = desc(kind.config_type(), &put(&config_bytes)?, config_bytes.len())?;
    let layer = desc(kind.content_type(), &put(&tar)?, tar.len())?;
    let manifest =
        osi::manifest(kind, &config_desc, std::slice::from_ref(&layer)).map_err(|e| e.to_string())?;
    let d = put(&manifest)?;
    Ok((desc(oci::media::OCI_MANIFEST, &d, manifest.len())?, config))
}

/// Every blob of the artifact `desc` names: its manifest, config and layers.
fn contents(store: &Store, desc: &Descriptor) -> Result<Vec<Digest>, String> {
    let (m, _) = manifest_of(store, desc)?;
    let mut out = vec![desc.digest().map_err(|e| e.to_string())?];
    for d in std::iter::once(&m.config).chain(&m.layers) {
        out.push(d.digest().map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// The artifact manifest `desc` names, here, and its kind.
pub fn manifest_of(store: &Store, desc: &Descriptor) -> Result<(oci::Manifest, Kind), String> {
    let bytes = store
        .content(desc, oci::MAX_MANIFEST)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{}: its manifest is not here", desc.digest))?;
    let Document::Manifest(m) = oci::parse_document(&bytes, &desc.media_type).map_err(|e| e.to_string())?
    else {
        return Err(format!(
            "{}: an index, where an OSI artifact's manifest was asked for",
            desc.digest
        ));
    };
    let kind = m.artifact_type.as_deref().and_then(Kind::of).ok_or_else(|| {
        format!(
            "{}: no OSI artifact (its artifactType is {})",
            desc.digest,
            m.artifact_type.as_deref().unwrap_or("none")
        )
    })?;
    if m.config.media_type != kind.config_type() {
        return Err(format!(
            "{}: its config is {}, not {}",
            desc.digest,
            m.config.media_type,
            kind.config_type()
        ));
    }
    for l in &m.layers {
        if osi::content_compression(kind, &l.media_type).is_none() {
            return Err(format!(
                "{}: a layer of type {}, which no {} holds",
                desc.digest,
                l.media_type,
                kind.word()
            ));
        }
    }
    Ok((m, kind))
}

/// The tar of `dir`'s files but `skip` at its root: names in order, owned by root,
/// times kept; refused if it holds what no domain may (§9.2).
fn pack(dir: &Path, skip: &[&str], out: &mut Vec<u8>) -> Result<(), String> {
    use shards_archive::tar::{self as gotar, Header};
    let root = fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut tw = gotar::Writer::new(out);
    let mut seen: BTreeMap<(u64, u64), String> = BTreeMap::new();
    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let mut names: Vec<OsString> = fs::read_dir(root.join(&rel))
            .map_err(|e| format!("{}: {e}", root.join(&rel).display()))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()
            .map_err(|e| e.to_string())?;
        names.sort();
        // Directories pushed in reverse, to be walked in name order after their parent.
        let mut dirs = Vec::new();
        for n in names {
            let path = rel.join(&n);
            let shown = path.to_string_lossy().replace('\\', "/");
            if rel.as_os_str().is_empty() && skip.iter().any(|s| *s == shown) {
                continue;
            }
            let full = root.join(&path);
            let md = fs::symlink_metadata(&full).map_err(|e| format!("{shown}: {e}"))?;
            let ft = md.file_type();
            let mut h = Header {
                name: shown.clone().into_bytes(),
                mode: mode_of(&md),
                mtime: gotar::Time::unix(mtime_of(&md), 0),
                ..Header::default()
            };
            if h.mode & 0o6000 != 0 {
                return Err(format!(
                    "{shown}: set-ID bits, which nothing in an agent's directory may have"
                ));
            }
            if ft.is_dir() {
                h.typeflag = gotar::TYPE_DIR;
                h.name.push(b'/');
                tw.write_header(&h).map_err(|e| e.to_string())?;
                dirs.push(path);
            } else if ft.is_symlink() {
                let target = fs::read_link(&full).map_err(|e| format!("{shown}: {e}"))?;
                if !stays_inside(&rel, &target) {
                    return Err(format!(
                        "{shown}: a symlink to {}, outside the directory",
                        target.display()
                    ));
                }
                h.typeflag = gotar::TYPE_SYMLINK;
                h.linkname = target.to_string_lossy().into_owned().into_bytes();
                tw.write_header(&h).map_err(|e| e.to_string())?;
            } else if ft.is_file() {
                if let Some(first) = link_key(&md).and_then(|k| seen.get(&k)) {
                    h.typeflag = gotar::TYPE_LINK;
                    h.linkname = first.clone().into_bytes();
                    tw.write_header(&h).map_err(|e| e.to_string())?;
                    continue;
                }
                if let Some(k) = link_key(&md) {
                    seen.insert(k, shown.clone());
                }
                h.typeflag = gotar::TYPE_REG;
                h.size = i64::try_from(md.len()).map_err(|e| e.to_string())?;
                tw.write_header(&h).map_err(|e| e.to_string())?;
                let f = fs::File::open(&full).map_err(|e| format!("{shown}: {e}"))?;
                tw.copy_from(f).map_err(|e| format!("{shown}: {e}"))?;
            } else {
                return Err(format!(
                    "{shown}: a device, FIFO or socket, which no agent's directory may hold"
                ));
            }
        }
        stack.extend(dirs.into_iter().rev());
    }
    tw.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Whether a symlink in `dir` (relative to the root) pointing at `target` resolves inside
/// the root, as written: absolute targets never do.
fn stays_inside(dir: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth = dir.components().count() as i64;
    for c in target.components() {
        match c {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

#[cfg(unix)]
fn mode_of(md: &fs::Metadata) -> i64 {
    use std::os::unix::fs::PermissionsExt;
    i64::from(md.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(md: &fs::Metadata) -> i64 {
    if md.is_dir() {
        0o755
    } else if md.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
fn link_key(md: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (md.nlink() > 1).then(|| (md.dev(), md.ino()))
}

#[cfg(not(unix))]
fn link_key(_: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn mtime_of(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Each content layer of a stored artifact read through, refusing what no domain may
/// hold: whiteouts, devices, FIFOs, set-ID bits, absolute or climbing names, and
/// symlinks or hard links leaving the directory.
pub fn check_content(store: &Store, kind: Kind, m: &oci::Manifest) -> Result<(), String> {
    for l in &m.layers {
        let d = l.digest().map_err(|e| e.to_string())?;
        let file = fs::File::open(store.blob_path(&d)).map_err(|e| format!("{d}: {e}"))?;
        let file = io::BufReader::new(file);
        let reader: Box<dyn Read> = match osi::content_compression(kind, &l.media_type) {
            Some(None) => Box::new(file),
            Some(Some("gzip")) => Box::new(shards_image::store::gunzip(file)),
            Some(Some("zstd")) => Box::new(shards_image::store::Zstd::new(file)),
            _ => return Err(format!("{d}: a layer of type {}", l.media_type)),
        };
        let mut r = shards_archive::tar::Reader::new(reader);
        while let Some(h) = r.next_header().map_err(|e| format!("{d}: {e}"))? {
            let name = String::from_utf8_lossy(&h.name).into_owned();
            let path = Path::new(name.trim_end_matches('/'));
            let base = path
                .file_name()
                .map(|b| b.to_string_lossy().into_owned())
                .unwrap_or_default();
            if base.starts_with(".wh.") {
                return Err(format!("{name}: a whiteout, which no OSI artifact holds"));
            }
            if path.is_absolute() || !stays_inside(Path::new(""), path) {
                return Err(format!("{name}: a name outside the artifact's directory"));
            }
            if h.mode & 0o6000 != 0 {
                return Err(format!(
                    "{name}: set-ID bits, which nothing in an agent's directory may have"
                ));
            }
            match h.typeflag {
                shards_archive::tar::TYPE_REG
                | shards_archive::tar::TYPE_REGA
                | shards_archive::tar::TYPE_DIR => {}
                shards_archive::tar::TYPE_SYMLINK => {
                    let target = PathBuf::from(String::from_utf8_lossy(&h.linkname).into_owned());
                    let dir = path.parent().unwrap_or(Path::new(""));
                    if !stays_inside(dir, &target) {
                        return Err(format!(
                            "{name}: a symlink to {}, outside the directory",
                            target.display()
                        ));
                    }
                }
                shards_archive::tar::TYPE_LINK => {
                    let target = PathBuf::from(String::from_utf8_lossy(&h.linkname).into_owned());
                    if !stays_inside(Path::new(""), &target) {
                        return Err(format!("{name}: a hard link outside the directory"));
                    }
                }
                _ => {
                    return Err(format!(
                        "{name}: a device, FIFO or other entry no agent's directory may hold"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The artifact `name` in the store, checked; pulled first if it is not here, or if
/// `fresh` says to. Returns its manifest's descriptor, manifest, kind and config.
pub fn fetch(
    store: &Store,
    name: &Reference,
    want: Kind,
    fresh: bool,
    say: &dyn Fn(&str),
) -> Result<(Descriptor, oci::Manifest, Config), String> {
    let held = if fresh {
        None
    } else {
        store.tagged(&name.to_string()).map_err(|e| e.to_string())?
    };
    let desc = match held {
        Some(d) => d,
        None => {
            say(&format!("pulling {name}"));
            let registry = crate::pull::registry(name, None, &|k| std::env::var(k).ok())?;
            let desc = registry.resolve(store, name).map_err(|e| e.to_string())?;
            let bytes = registry.fetch_document(store, &desc).map_err(|e| e.to_string())?;
            // An index of several platforms: this host's manifest, the name resolving to
            // the index.
            let (desc, bytes, index) =
                match oci::parse_document(&bytes, &desc.media_type).map_err(|e| e.to_string())? {
                    Document::Index(index) => {
                        let chosen = shards_image::platform::select(&index, &shards_image::platform::guest())
                            .cloned()
                            .ok_or_else(|| format!("{name}: no {} for this platform", want.word()))?;
                        let bytes = registry
                            .fetch_document(store, &chosen)
                            .map_err(|e| e.to_string())?;
                        (
                            Descriptor {
                                platform: None,
                                ..chosen
                            },
                            bytes,
                            Some(desc.digest().map_err(|e| e.to_string())?),
                        )
                    }
                    Document::Manifest(_) => (desc, bytes, None),
                };
            let Document::Manifest(m) =
                oci::parse_document(&bytes, &desc.media_type).map_err(|e| e.to_string())?
            else {
                return Err(format!("{name}: an index inside an index"));
            };
            let limits = crate::pull::limits()?;
            for part in std::iter::once(&m.config).chain(&m.layers) {
                registry
                    .fetch_blob(store, part, &limits, &|_| {})
                    .map_err(|e| e.to_string())?;
            }
            let digest = desc.digest().map_err(|e| e.to_string())?;
            let (_, kind) = manifest_of(store, &desc)?;
            if kind != want {
                return Err(format!("{name} is {}, not {}", a(kind), a(want)));
            }
            let mut held = contents(store, &desc)?;
            held.extend(index.clone());
            store
                .tag(&name.to_string(), &desc, index.as_ref().unwrap_or(&digest), &held)
                .map_err(|e| e.to_string())?;
            desc
        }
    };
    let (m, kind) = manifest_of(store, &desc)?;
    if kind != want {
        return Err(format!("{name} is {}, not {}", a(kind), a(want)));
    }
    let config = store
        .content(&m.config, oci::MAX_CONFIG)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{name}: its config is not here"))?;
    let config = Config::parse(&config).map_err(|e| format!("{name}: {e}"))?;
    check_content(store, kind, &m).map_err(|e| format!("{name}: {e}"))?;
    Ok((desc, m, config))
}

fn pull_cmd(kind: Kind, args: &[String]) -> Result<(), String> {
    let [name] = args else { return Err(usage(kind)) };
    let name = reference(name)?;
    let store = store()?;
    let _lease = store.lease().map_err(|e| e.to_string())?;
    let (desc, _, config) = fetch(&store, &name, kind, true, &|l| {
        let _ = writeln!(io::stderr(), "{l}");
    })?;
    let _ = writeln!(
        io::stdout(),
        "{name}: {} {} {}",
        kind.word(),
        config.name,
        desc.digest
    );
    Ok(())
}

fn push(kind: Kind, args: &[String]) -> Result<(), String> {
    let [name] = args else { return Err(usage(kind)) };
    let name = reference(name)?;
    let store = store()?;
    let _lease = store.lease().map_err(|e| e.to_string())?;
    let desc = store
        .tagged(&name.to_string())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no {} {name} here", kind.word()))?;
    let (m, found) = manifest_of(&store, &desc)?;
    if found != kind {
        return Err(format!("{name} is {}, not {}", a(found), a(kind)));
    }
    check_content(&store, kind, &m)?;
    let registry = crate::pull::registry_for_push(&name, None, None, &|k| std::env::var(k).ok())?;
    // Of several platforms, the index: every platform's manifest with it.
    let resolved = store.resolved(&name.to_string()).map_err(|e| e.to_string())?;
    let desc = match resolved.filter(|r| r.to_string() != desc.digest) {
        Some(index) => {
            let size = std::fs::metadata(store.blob_path(&index))
                .map_err(|e| format!("{index}: {e}"))?
                .len();
            Descriptor {
                media_type: oci::media::OCI_INDEX.into(),
                digest: index.to_string(),
                size: i64::try_from(size).map_err(|e| e.to_string())?,
                platform: None,
                annotations: BTreeMap::new(),
            }
        }
        None => desc,
    };
    shards_registry::push::push(&registry, &store, &desc, name.tag.as_deref(), None, &|_, _| {})
        .map_err(|e| e.to_string())?;
    let _ = writeln!(io::stdout(), "{name}: pushed {}", desc.digest);
    Ok(())
}

/// The OSI artifacts of `kind` the store holds: each name, its config's name, version and
/// manifest digest.
fn list(kind: Kind) -> Result<(), String> {
    let store = store()?;
    let mut rows = Vec::new();
    for (name, _) in store.references().map_err(|e| e.to_string())? {
        let Some(desc) = store.tagged(&name).map_err(|e| e.to_string())? else {
            continue;
        };
        let Ok((m, k)) = manifest_of(&store, &desc) else {
            continue;
        };
        if k != kind {
            continue;
        }
        let version = store
            .content(&m.config, oci::MAX_CONFIG)
            .ok()
            .flatten()
            .and_then(|c| Config::parse(&c).ok())
            .map(|c| c.version)
            .unwrap_or_default();
        rows.push((name, version, desc.digest.clone()));
    }
    let _ = writeln!(io::stdout(), "NAME\tVERSION\tDIGEST");
    for (n, v, d) in rows {
        let _ = writeln!(io::stdout(), "{n}\t{v}\t{d}");
    }
    Ok(())
}

fn inspect(kind: Kind, args: &[String]) -> Result<(), String> {
    let [name] = args else { return Err(usage(kind)) };
    let name = reference(name)?;
    let store = store()?;
    let desc = store
        .tagged(&name.to_string())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no {} {name} here", kind.word()))?;
    let (m, found) = manifest_of(&store, &desc)?;
    if found != kind {
        return Err(format!("{name} is {}, not {}", a(found), a(kind)));
    }
    let config = store
        .content(&m.config, oci::MAX_CONFIG)
        .map_err(|e| e.to_string())?
        .ok_or("its config is not here")?;
    let config: serde_json::Value = serde_json::from_slice(&config).map_err(|e| e.to_string())?;
    let out = serde_json::json!([{
        "Name": name.to_string(),
        "Digest": desc.digest,
        "ArtifactType": kind.artifact_type(),
        "Config": config,
        "Layers": m.layers,
    }]);
    let _ = writeln!(
        io::stdout(),
        "{}",
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn remove(kind: Kind, args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err(usage(kind));
    }
    let store = store()?;
    for n in args {
        let name = reference(n)?;
        let desc = store
            .tagged(&name.to_string())
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no {} {name} here", kind.word()))?;
        let (_, found) = manifest_of(&store, &desc)?;
        if found != kind {
            return Err(format!("{name} is {}, not {}", a(found), a(kind)));
        }
        store.untag(&name.to_string()).map_err(|e| e.to_string())?;
        let _ = writeln!(io::stdout(), "Untagged: {name}");
    }
    Ok(())
}

/// The image layer type an OSI content layer is read as: the same tar, compressed the
/// same way, whiteouts already refused (check_content).
pub fn layer_type(kind: Kind, media_type: &str) -> Option<&'static str> {
    match osi::content_compression(kind, media_type)? {
        None => Some("application/vnd.oci.image.layer.v1.tar"),
        Some("gzip") => Some("application/vnd.oci.image.layer.v1.tar+gzip"),
        Some(_) => Some("application/vnd.oci.image.layer.v1.tar+zstd"),
    }
}

/// Each content layer's diff ID: the SHA-256 of its tar as uncompressed.
pub fn diff_ids(store: &Store, kind: Kind, m: &oci::Manifest) -> Result<Vec<Digest>, String> {
    let mut out = Vec::new();
    for l in &m.layers {
        let d = l.digest().map_err(|e| e.to_string())?;
        let file = io::BufReader::new(fs::File::open(store.blob_path(&d)).map_err(|e| format!("{d}: {e}"))?);
        let mut reader: Box<dyn Read> = match osi::content_compression(kind, &l.media_type) {
            Some(None) => Box::new(file),
            Some(Some("gzip")) => Box::new(shards_image::store::gunzip(file)),
            Some(Some("zstd")) => Box::new(shards_image::store::Zstd::new(file)),
            _ => return Err(format!("{d}: a layer of type {}", l.media_type)),
        };
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = reader.read(&mut buf).map_err(|e| format!("{d}: {e}"))?;
            if n == 0 {
                break;
            }
            h.update(buf.get(..n).unwrap_or_default());
        }
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        out.push(Digest::parse(&format!("sha256:{hex}")).map_err(|e| e.to_string())?);
    }
    Ok(out)
}
