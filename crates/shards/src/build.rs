//! `shards build`: builds an image as `docker build` does (docs/design/architecture.md
//! D33). The command line is buildx's (`shards_cmdline::commands::BUILD`); the plan is
//! BuildKit's (`shards_dockerfile::plan`); the image is written to this user's store as
//! BuildKit's exporter writes it (`shards_dockerfile::export`), so `shards run` boots it.
//! Progress is BuildKit's plain display.
//!
//! What runs so far: stages of base images and of configuration alone (FROM, ENV, LABEL,
//! CMD and the rest). A step that changes files (RUN, COPY, ADD, WORKDIR other than `/`)
//! fails with a message that says so.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use shards_cmdline::commands::BUILD;
use shards_cmdline::flags::{self, Outcome, Parsed};
use shards_dockerfile::export::{self, Layer};
use shards_dockerfile::go::Time;
use shards_dockerfile::image::Image;
use shards_dockerfile::llb::OpKind;
use shards_dockerfile::plan::{self, Options, Resolved, Resolver};
use shards_dockerfile::platform::{self, Platform};
use shards_image::oci::{self, Descriptor, Document};
use shards_image::platform as image_platform;
use shards_image::reference::{Algorithm, Digest, Reference};
use shards_image::store::{self, Store};
use shards_registry::pull::{self as registry_pull, Event};

const PATH: &str = "shards buildx build";

/// The yellow buildx writes warnings in (aec.YellowF), whatever the output.
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

pub fn build(args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut argv = Vec::new();
    for a in args {
        match a.into_string() {
            Ok(a) => argv.push(a),
            Err(a) => return failed(&format!("{a:?} is not UTF-8")),
        }
    }
    let parsed = match flags::parse(&BUILD, PATH, &argv, &|_, value| Ok(value.to_string())) {
        Outcome::Run(parsed) => parsed,
        Outcome::Help { notices } => {
            let _ = write!(std::io::stdout(), "{notices}{}", flags::help(&BUILD, PATH, 80));
            return ExitCode::SUCCESS;
        }
        Outcome::Fail {
            notices,
            text,
            status,
        } => {
            let _ = write!(std::io::stdout(), "{notices}");
            let _ = writeln!(std::io::stderr(), "{text}");
            return ExitCode::from(status);
        }
    };
    let _ = write!(std::io::stdout(), "{}", parsed.notices);
    match run(&parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "ERROR: {message}");
    ExitCode::FAILURE
}

/// BuildKit's plain progress display, one vertex after another (progressui's printer, as
/// each vertex completes before the next starts).
struct Progress {
    quiet: bool,
    next: usize,
}

struct Vertex {
    index: usize,
    started: Instant,
}

impl Progress {
    fn say(&self, text: &str) {
        if !self.quiet {
            let _ = write!(std::io::stderr(), "{text}");
        }
    }

    /// A vertex begins: a blank line after the one before, as progressui separates them.
    fn start(&mut self, name: &str) -> Vertex {
        self.next += 1;
        self.say(&format!("\n#{} {name}\n", self.next));
        Vertex {
            index: self.next,
            started: Instant::now(),
        }
    }

    fn line(&self, v: &Vertex, text: &str) {
        self.say(&format!("#{} {text}\n", v.index));
    }

    fn done(&self, v: &Vertex) {
        let secs = v.started.elapsed().as_secs_f64();
        self.say(&format!("#{} DONE {secs:.1}s\n", v.index));
    }

    fn error(&self, v: &Vertex, message: &str) {
        self.say(&format!("#{} ERROR: {message}\n", v.index));
    }
}

/// A base image as the build resolved it.
struct Base {
    reference: Reference,
    image: Image,
    layers: Vec<Layer>,
}

/// Resolves base images from this user's store, pulling those it lacks (or every one,
/// with `--pull`), as the docker driver prefers its local images.
struct Bases<'a> {
    home: &'a Path,
    store: &'a Store,
    pull: bool,
    progress: &'a RefCell<Progress>,
    resolved: RefCell<BTreeMap<String, Base>>,
}

impl Resolver for Bases<'_> {
    fn resolve(&self, name: &[u8], platform: &Platform) -> Result<Resolved, Vec<u8>> {
        let name = String::from_utf8_lossy(name).into_owned();
        let v = self
            .progress
            .borrow_mut()
            .start(&format!("[internal] load metadata for {name}"));
        let r = self.fetch(&name, platform);
        let progress = self.progress.borrow();
        match &r {
            Ok(_) => progress.done(&v),
            Err(e) => progress.error(&v, &String::from_utf8_lossy(e)),
        }
        r
    }
}

impl Bases<'_> {
    fn fetch(&self, name: &str, wanted: &Platform) -> Result<Resolved, Vec<u8>> {
        let fail = |e: String| e.into_bytes();
        let host = host_platform();
        if wanted.os != host.os || wanted.architecture != host.architecture {
            return Err(fail(format!(
                "shards builds for {} only, the platform its microVMs run: not {}",
                show(&platform::format(&host)),
                show(&platform::format(wanted)),
            )));
        }
        let reference = Reference::parse(name).map_err(|e| fail(e.to_string()))?;
        let limits = crate::pull::limits(self.home).map_err(fail)?;
        let local = if self.pull {
            None
        } else {
            registry_pull::local(self.store, &reference, &image_platform::guest(), &limits)
                .map_err(|e| fail(e.to_string()))?
        };
        let pulled = match local {
            Some(p) => p,
            None => {
                crate::pull::fetch(self.home, &reference, &|_: Event<'_>| {}, &|_| {}, None)
                    .map_err(fail)?
                    .0
            }
        };
        // The manifest and config as stored, for the layers' descriptors and the config's
        // bytes.
        let record = self
            .store
            .tagged(&reference.to_string())
            .map_err(|e| fail(e.to_string()))?
            .ok_or_else(|| fail(format!("{name}: not in the store after its pull")))?;
        let manifest_bytes = self
            .store
            .content(&record, oci::MAX_MANIFEST)
            .map_err(|e| fail(e.to_string()))?
            .ok_or_else(|| fail(format!("{name}: its manifest is missing")))?;
        let Document::Manifest(manifest) =
            oci::parse_document(&manifest_bytes, &record.media_type).map_err(|e| fail(e.to_string()))?
        else {
            return Err(fail(format!("{name}: its record names an index")));
        };
        let config = self
            .store
            .content(&manifest.config, oci::MAX_CONFIG)
            .map_err(|e| fail(e.to_string()))?
            .ok_or_else(|| fail(format!("{name}: its config is missing")))?;
        let image = Image::from_json(&config).map_err(|e| fail(format!("{name}: {}", show(&e))))?;
        let annotations = layer_annotations(&manifest_bytes);
        let diff_ids = image.rootfs.diff_ids.clone().unwrap_or_default();
        let mut layers = Vec::new();
        for (i, l) in manifest.layers.iter().enumerate() {
            let digest = l.digest().map_err(|e| fail(e.to_string()))?;
            layers.push(Layer {
                media_type: l.media_type.as_bytes().to_vec(),
                digest: l.digest.as_bytes().to_vec(),
                size: u64::try_from(l.size)
                    .map_err(|_| fail(format!("{name}: a layer's size is negative")))?,
                diff_id: diff_ids.get(i).cloned().unwrap_or_default(),
                annotations: annotations.get(i).cloned().unwrap_or_default(),
                // When the store first held the layer, as BuildKit's records time a pull.
                created: stored_at(&self.store.blob_path(&digest)),
                description: format!("pulled from {reference}").into_bytes(),
            });
        }
        let resolved = Resolved {
            reference: reference.to_string().into_bytes(),
            digest: Some(pulled.resolved.to_string().into_bytes()),
            config,
        };
        self.resolved.borrow_mut().insert(
            name.to_string(),
            Base {
                reference,
                image,
                layers,
            },
        );
        Ok(resolved)
    }
}

/// Each layer's annotations, from the manifest's own JSON.
fn layer_annotations(manifest: &[u8]) -> Vec<BTreeMap<Vec<u8>, Vec<u8>>> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(manifest) else {
        return Vec::new();
    };
    v.get("layers")
        .and_then(|l| l.as_array())
        .map(|layers| {
            layers
                .iter()
                .map(|l| {
                    l.get("annotations")
                        .and_then(|a| a.as_object())
                        .map(|a| {
                            a.iter()
                                .filter_map(|(k, v)| {
                                    Some((k.as_bytes().to_vec(), v.as_str()?.as_bytes().to_vec()))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// When `path` was last changed, in UTC: a stored blob's arrival.
fn stored_at(path: &Path) -> Option<Time> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    let mut t = Time::from_unix(i64::try_from(since.as_secs()).ok()?);
    t.nanosecond = since.subsec_nanos();
    Some(t)
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The platform this host's microVMs run.
fn host_platform() -> Platform {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    Platform::new("linux", arch)
}

/// `KEY=VALUE` arguments as buildx reads them: a `KEY` alone takes its value from the
/// environment, and is left out when unset (`listToMap`); a `KEY=VALUE` label alone is
/// `KEY` with an empty value.
fn build_args(list: &[String], from_env: bool) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut out = BTreeMap::new();
    for item in list {
        match item.split_once('=') {
            Some((k, v)) => {
                out.insert(k.as_bytes().to_vec(), v.as_bytes().to_vec());
            }
            None if from_env => {
                if let Ok(v) = std::env::var(item) {
                    out.insert(item.as_bytes().to_vec(), v.into_bytes());
                }
            }
            None => {
                out.insert(item.as_bytes().to_vec(), Vec::new());
            }
        }
    }
    out
}

/// The Dockerfile and its name: `-f`'s file, `-` for stdin, or PATH's `Dockerfile`.
fn dockerfile(parsed: &Parsed, context: &Path) -> Result<(String, Vec<u8>), String> {
    let file = parsed.string("file");
    if file == "-" {
        let mut text = Vec::new();
        std::io::stdin()
            .read_to_end(&mut text)
            .map_err(|e| format!("reading the Dockerfile from stdin: {e}"))?;
        return Ok(("Dockerfile".into(), text));
    }
    let path = if file.is_empty() {
        context.join("Dockerfile")
    } else {
        PathBuf::from(file)
    };
    let name = path
        .file_name()
        .map_or_else(|| "Dockerfile".into(), |n| n.to_string_lossy().into_owned());
    let text = std::fs::read(&path).map_err(|e| format!("failed to read dockerfile: open {name}: {e}"))?;
    Ok((name, text))
}

/// BuildKit's excerpt of the Dockerfile at an error's lines (solver/errdefs Source.Print).
fn excerpt(file: &str, text: &[u8], ranges: &[(usize, usize)]) -> String {
    let text = String::from_utf8_lossy(text);
    let lines: Vec<&str> = text.split('\n').collect();
    let Some(start) = ranges.iter().map(|r| r.0).min() else {
        return String::new();
    };
    let mut end = ranges.iter().map(|r| r.1.max(r.0)).max().unwrap_or(start);
    if start > lines.len() || start < 1 {
        return String::new();
    }
    end = end.min(lines.len());
    let pad = if end == start { 4 } else { 2 };
    let (mut first, mut p) = (start, 0);
    while p < pad {
        if first > 1 {
            first -= 1;
            p += 1;
        }
        if end != lines.len() {
            end += 1;
            p += 1;
        }
        p += 1;
    }
    let mut out = format!("{file}:{start}\n--------------------\n");
    for i in first..=end {
        let marked = ranges.iter().any(|r| r.0 <= i && r.1.max(r.0) >= i);
        let pfx = if marked { ">>>" } else { "   " };
        out.push_str(&format!(" {i:>3} | {pfx} {}\n", lines.get(i - 1).unwrap_or(&"")));
    }
    out.push_str("--------------------\n");
    out
}

fn run(parsed: &Parsed) -> Result<(), String> {
    let context_arg = parsed.args.first().cloned().unwrap_or_default();
    if context_arg == "-" || context_arg.contains("://") || context_arg.starts_with("git@") {
        return Err(format!(
            "{context_arg:?}: shards builds from a directory context only, so far"
        ));
    }
    let context = PathBuf::from(&context_arg);
    if !context.is_dir() {
        return Err(format!(
            "unable to prepare context: path {context_arg:?} not found"
        ));
    }
    let mode = match parsed.string("progress") {
        "auto" | "plain" | "tty" => "plain",
        "quiet" | "none" => "quiet",
        "rawjson" => return Err("--progress=rawjson is not supported by shards yet".into()),
        other => return Err(format!("invalid progress mode {other:?}")),
    };
    let quiet = parsed.bool("quiet") || mode == "quiet";
    let host = host_platform();
    for p in parsed.many("platform") {
        for one in p.split(',') {
            let wanted = platform::parse(one.trim().as_bytes(), &host).map_err(|e| show(&e))?;
            if wanted.os != host.os || wanted.architecture != host.architecture {
                return Err(format!(
                    "shards builds for {} only, the platform its microVMs run: not {}",
                    show(&platform::format(&host)),
                    show(&platform::format(&wanted)),
                ));
            }
        }
    }
    let progress = RefCell::new(Progress { quiet, next: 0 });
    progress
        .borrow()
        .say("#0 building with \"shards\" instance using shards driver\n");

    let (name, text) = {
        let v = progress
            .borrow_mut()
            .start("[internal] load build definition from Dockerfile");
        match dockerfile(parsed, &context) {
            Ok((name, text)) => {
                let p = progress.borrow();
                p.line(&v, &format!("transferring dockerfile: {}B done", text.len()));
                p.done(&v);
                (name, text)
            }
            Err(e) => {
                progress.borrow().error(&v, &e);
                return Err(format!("failed to build: failed to solve: {e}"));
            }
        }
    };

    let home = shards_ipc::home()?;
    let store = crate::pull::store(&home)?;
    let _lease = store.lease().map_err(|e| e.to_string())?;
    let bases = Bases {
        home: &home,
        store: &store,
        pull: parsed.bool("pull"),
        progress: &progress,
        resolved: RefCell::new(BTreeMap::new()),
    };
    let opts = Options {
        target_platform: host.clone(),
        build_platforms: vec![host],
        build_args: build_args(parsed.many("build-arg"), true),
        target: parsed.string("target").as_bytes().to_vec(),
        labels: build_args(parsed.many("label"), false),
        hostname: Vec::new(),
        multi_platform: false,
        context_id: format!("shards-{}", std::process::id()).into_bytes(),
    };
    let plan = match plan::plan(&text, &opts, &bases) {
        Ok(p) => p,
        Err(e) => {
            let mut out = String::new();
            for loc in &e.location {
                out.push_str(&excerpt(&name, &text, loc));
            }
            let _ = write!(std::io::stderr(), "{out}");
            print_warnings(&e.warnings, quiet);
            return Err(format!("failed to build: failed to solve: {}", show(&e.message)));
        }
    };
    {
        let v = progress.borrow_mut().start("[internal] load .dockerignore");
        progress.borrow().done(&v);
    }

    // The steps: base images resolved; anything else is for the steps to come.
    let def = plan.definition();
    let mut layers: Vec<Layer> = Vec::new();
    let mut base_image: Option<Image> = None;
    for (op, meta) in def.ops.iter().zip(&def.metadata) {
        let name = meta
            .description
            .get(b"llb.customname".as_slice())
            .map(|n| show(n))
            .unwrap_or_default();
        match &op.kind {
            OpKind::Source { identifier, .. } if identifier.starts_with(b"docker-image://") => {
                let v = progress.borrow_mut().start(&name);
                let reference = show(identifier.strip_prefix(b"docker-image://").unwrap_or(identifier));
                progress.borrow().line(&v, &format!("resolve {reference} done"));
                progress.borrow().done(&v);
            }
            OpKind::Source { identifier, .. } if identifier.starts_with(b"local://") => {}
            _ => {
                let what = name.split_once("] ").map_or(name.as_str(), |(_, s)| s);
                let v = progress.borrow_mut().start(&name);
                let why = format!("{what}: this step is not supported by shards build yet");
                progress.borrow().error(&v, &why);
                print_warnings(&plan.warnings, quiet);
                return Err(format!("failed to build: failed to solve: {why}"));
            }
        }
    }
    // The image's layers: its base's, when the target's root is an image.
    if let Some(root) = def.root
        && let Some(op) = def.ops.get(root.op)
        && let OpKind::Source { identifier, .. } = &op.kind
    {
        let reference = show(identifier.strip_prefix(b"docker-image://").unwrap_or(identifier));
        let resolved = bases.resolved.borrow();
        let base = resolved
            .values()
            .find(|b| reference.starts_with(&b.reference.to_string()) || reference == b.reference.to_string())
            .ok_or_else(|| format!("{reference}: not resolved"))?;
        layers = base.layers.clone();
        base_image = Some(base.image.clone());
    }

    let v = progress.borrow_mut().start("exporting to image");
    let epoch = plan.epoch.map(Time::from_unix);
    let config = export::config(&plan.image, &layers, epoch, base_image.as_ref()).map_err(|e| show(&e))?;
    let config_digest = sha256(&config);
    let manifest = export::manifest(&config, config_digest.to_string().as_bytes(), &layers);
    let manifest_digest = sha256(&manifest);
    progress.borrow().line(&v, "exporting layers done");
    store
        .ingest(&config_digest, config.len() as u64, &mut config.as_slice())
        .map_err(|e| e.to_string())?;
    store
        .ingest(&manifest_digest, manifest.len() as u64, &mut manifest.as_slice())
        .map_err(|e| e.to_string())?;
    progress
        .borrow()
        .line(&v, &format!("writing image {config_digest} done"));
    let store_layers: Vec<store::Layer> = layers
        .iter()
        .map(|l| {
            Ok(store::Layer {
                blob: Digest::parse(&show(&l.digest)).map_err(|e| e.to_string())?,
                media_type: show(&l.media_type),
                diff_id: Digest::parse(&show(&l.diff_id)).map_err(|e| e.to_string())?,
            })
        })
        .collect::<Result<_, String>>()?;
    if !store_layers.is_empty() {
        let limits = crate::pull::limits(&home)?;
        store.rootfs(&store_layers, &limits).map_err(|e| e.to_string())?;
    }
    let desc = Descriptor {
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        digest: manifest_digest.to_string(),
        size: i64::try_from(manifest.len()).map_err(|e| e.to_string())?,
        platform: None,
    };
    let mut contents = vec![manifest_digest.clone(), config_digest.clone()];
    contents.extend(store_layers.iter().map(|l| l.blob.clone()));
    for tag in parsed.many("tag") {
        let reference = Reference::parse(tag).map_err(|e| format!("invalid tag {tag:?}: {e}"))?;
        store
            .tag(&reference.to_string(), &desc, &contents)
            .map_err(|e| e.to_string())?;
        progress.borrow().line(&v, &format!("naming to {reference} done"));
    }
    progress.borrow().done(&v);
    print_warnings(&plan.warnings, quiet);

    let id = config_digest.to_string();
    let iidfile = parsed.string("iidfile");
    if !iidfile.is_empty() {
        std::fs::write(iidfile, &id).map_err(|e| format!("{iidfile}: {e}"))?;
    }
    if parsed.bool("quiet") {
        let _ = writeln!(std::io::stdout(), "{id}");
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> Digest {
    use sha2::Digest as _;
    Digest::from_hash(Algorithm::Sha256, &sha2::Sha256::digest(bytes))
}

/// buildx's warnings after a build (commands/build.go printWarnings).
fn print_warnings(warnings: &[shards_dockerfile::lint::Warning], quiet: bool) {
    if warnings.is_empty() || quiet {
        return;
    }
    let count = if warnings.len() == 1 {
        "1 warning found".to_string()
    } else {
        format!("{} warnings found", warnings.len())
    };
    let mut out = format!("\n {YELLOW}{count} (use shards --debug to expand):\n{RESET}");
    for w in warnings {
        let line = w.location.first().map_or(0, |r| r.0);
        let mut short = format!("{}: {}", w.rule, show(&w.message));
        if line > 0 {
            short.push_str(&format!(" (line {line})"));
        }
        out.push_str(&format!(" - {short}\n"));
    }
    let _ = write!(std::io::stderr(), "{out}");
}
