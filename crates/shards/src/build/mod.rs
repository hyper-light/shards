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

mod builder;
mod exec;
pub(crate) mod step;

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

    /// A step's output, as progressui's plain mode prints it: each line with the seconds
    /// since its step began; a line not yet ended waits in `held` for the rest.
    fn log(&self, v: &Vertex, held: &mut Vec<u8>, bytes: &[u8]) {
        held.extend_from_slice(bytes);
        while let Some(end) = held.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = held.drain(..=end).collect();
            let text = String::from_utf8_lossy(line.strip_suffix(b"\n").unwrap_or(&line)).into_owned();
            let secs = v.started.elapsed().as_secs_f64();
            self.say(&format!("#{} {secs:.3} {text}\n", v.index));
        }
    }

    /// What a step left without a newline, once it ends.
    fn flush(&self, v: &Vertex, held: &mut Vec<u8>) {
        if !held.is_empty() {
            held.push(b'\n');
            self.log(v, held, &[]);
        }
    }
}

/// How many vCPUs a builder has: the host's, as BuildKit's steps may use them all.
fn builder_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// A builder's memory, in MiB: `SHARDS_BUILD_MEMORY` if set, else half the host's, as
/// Docker Desktop's VM has on macOS (64 GiB of this host's 128, measured 2026-10-02), so
/// that a build has what it would have under Docker. Its steps and its layers share it.
/// What it costs at boot grows with it (PM M82), until memory is plugged as a build
/// needs it.
fn builder_memory_mib() -> u64 {
    if let Some(m) = std::env::var("SHARDS_BUILD_MEMORY")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return m;
    }
    host_memory().map_or(512, |bytes| (bytes / 2 / (1 << 20)).max(512))
}

/// The host's physical memory, in bytes.
#[cfg(unix)]
fn host_memory() -> Option<u64> {
    // SAFETY: sysconf(3) with constant arguments.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    u64::try_from(pages).ok()?.checked_mul(u64::try_from(size).ok()?)
}

/// Where shards starts no builder yet (`builder::Builder`), nothing asks.
#[cfg(not(unix))]
fn host_memory() -> Option<u64> {
    None
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
        let limits = crate::pull::limits().map_err(fail)?;
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

/// A file's bytes, or none if it is not there. Unlike BuildKit, which has the client send
/// the Dockerfile and ignore files over a session, shards reads them where they are.
fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A Dockerfile's name, its text, and the ignore file beside it.
type Dockerfile = (String, Vec<u8>, Option<Vec<u8>>);

/// The Dockerfile, found as the frontend finds it (dockerui's Client.ReadEntrypoint):
/// `-f`'s file, stdin's, or PATH's `Dockerfile`, then `dockerfile` for the default name.
/// Its name, text, and the `<name>.dockerignore` beside it, which overrides the
/// context's `.dockerignore`.
fn dockerfile(parsed: &Parsed, context: &Path) -> Result<Dockerfile, String> {
    let file = parsed.string("file");
    if file == "-" {
        let mut text = Vec::new();
        std::io::stdin()
            .read_to_end(&mut text)
            .map_err(|e| format!("reading the Dockerfile from stdin: {e}"))?;
        return Ok(("Dockerfile".into(), text, None));
    }
    let path = if file.is_empty() {
        context.join("Dockerfile")
    } else {
        PathBuf::from(file)
    };
    let name = path
        .file_name()
        .map_or_else(|| "Dockerfile".into(), |n| n.to_string_lossy().into_owned());
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut text = read_if_present(&path)?;
    if text.is_none() && name == "Dockerfile" {
        text = read_if_present(&dir.join("dockerfile"))?;
    }
    let Some(text) = text else {
        return Err(format!(
            "failed to read dockerfile: open {name}: no such file or directory"
        ));
    };
    let beside = read_if_present(&dir.join(format!("{name}.dockerignore")))?;
    Ok((name, text, beside))
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
    crate::phase("start");
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

    let (name, text, beside) = {
        let v = progress
            .borrow_mut()
            .start("[internal] load build definition from Dockerfile");
        match dockerfile(parsed, &context) {
            Ok((name, text, ignore)) => {
                let p = progress.borrow();
                p.line(&v, &format!("read {} done", human_size(text.len() as u64)));
                p.done(&v);
                (name, text, ignore)
            }
            Err(e) => {
                progress.borrow().error(&v, &e);
                return Err(format!("failed to build: failed to solve: {e}"));
            }
        }
    };

    // The context's .dockerignore, unless one is beside the Dockerfile; its vertex shows
    // after the metadata, as the frontend reads it once the stages are planned.
    let context_ignore = match &beside {
        Some(_) => None,
        None => Some(read_if_present(&context.join(".dockerignore"))?),
    };
    let (ignore_name, ignore_text) = match (&beside, &context_ignore) {
        (Some(t), _) => (format!("{name}.dockerignore"), Some(t.clone())),
        (None, Some(t)) => (".dockerignore".to_string(), t.clone()),
        (None, None) => (String::new(), None),
    };
    let excludes = match ignore_text {
        Some(t) if !t.is_empty() => shards_dockerfile::ignore::read_all(&t).map_err(|e| {
            format!(
                "failed to build: failed to solve: failed to read dockerignore patterns: failed parsing {ignore_name}: {}",
                show(&e)
            )
        })?,
        _ => Vec::new(),
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
        excludes,
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
        if let Some(found) = &context_ignore {
            let v = progress.borrow_mut().start("[internal] load .dockerignore");
            if let Some(t) = found {
                progress
                    .borrow()
                    .line(&v, &format!("read {} done", human_size(t.len() as u64)));
            }
            progress.borrow().done(&v);
        }
    }

    // The steps, each after what it reads: base images and the context as snapshots,
    // file operations and merges run here; RUN is for the steps to come.
    let def = plan.definition();
    let limits = crate::pull::limits()?;
    crate::phase("plan");
    let mut exec = exec::Exec::new(&store, &limits);
    // The base images a RUN stands on: each a builder's pmem device, its root filesystem
    // as the store keeps it, so that nothing of it is copied into the builder.
    let mut run_images: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
    let mut run_bases: Vec<PathBuf> = Vec::new();
    {
        let mut todo: Vec<usize> = def
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op.kind, OpKind::Exec { .. }))
            .map(|(i, _)| i)
            .collect();
        let mut seen = std::collections::HashSet::new();
        while let Some(i) = todo.pop() {
            if !seen.insert(i) {
                continue;
            }
            let Some(op) = def.ops.get(i) else { continue };
            if let OpKind::Source { identifier, .. } = &op.kind {
                if let Some(reference) = identifier.strip_prefix(b"docker-image://") {
                    let reference = show(reference);
                    let layers = bases
                        .resolved
                        .borrow()
                        .values()
                        .find(|b| reference.starts_with(&b.reference.to_string()))
                        .map(|b| b.layers.clone())
                        .ok_or_else(|| format!("{reference}: not resolved"))?;
                    if !layers.is_empty() {
                        let store_layers = layers
                            .iter()
                            .map(|l| {
                                Ok(store::Layer {
                                    blob: Digest::parse(&show(&l.digest)).map_err(|e| e.to_string())?,
                                    media_type: show(&l.media_type),
                                    diff_id: Digest::parse(&show(&l.diff_id)).map_err(|e| e.to_string())?,
                                })
                            })
                            .collect::<Result<Vec<_>, String>>()?;
                        let rootfs = store.rootfs(&store_layers, &limits).map_err(|e| e.to_string())?;
                        let n = u32::try_from(run_bases.len()).map_err(|_| "too many base images")?;
                        run_bases.push(rootfs);
                        run_images.insert(i, n);
                    }
                }
                continue;
            }
            todo.extend(op.inputs.iter().map(|inp| inp.op));
        }
    }
    let mut builder: Option<builder::Builder> = None;
    let mut results: Vec<Vec<exec::Ref>> = Vec::with_capacity(def.ops.len());
    // What other operations read, so a base image is unpacked only when one does.
    let read: std::collections::HashSet<usize> = def
        .ops
        .iter()
        .filter(|op| !matches!(op.kind, OpKind::Source { .. }))
        .flat_map(|op| op.inputs.iter().map(|i| i.op))
        .collect();
    for (i, (op, meta)) in def.ops.iter().zip(&def.metadata).enumerate() {
        let name = meta
            .description
            .get(b"llb.customname".as_slice())
            .map(|n| show(n))
            .unwrap_or_default();
        let inputs = op
            .inputs
            .iter()
            .map(|inp| {
                results
                    .get(inp.op)
                    .and_then(|outs| outs.get(usize::try_from(inp.index).ok()?))
                    .cloned()
                    .ok_or_else(|| format!("input {}:{} is not ready", inp.op, inp.index))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let fail = |v: &Vertex, why: &str| {
            progress.borrow().error(v, why);
            print_warnings(&plan.warnings, quiet);
            format!("failed to build: failed to solve: {why}")
        };
        let outs = match &op.kind {
            OpKind::Source { identifier, .. } if identifier.starts_with(b"docker-image://") => {
                let v = progress.borrow_mut().start(&name);
                let reference = show(identifier.strip_prefix(b"docker-image://").unwrap_or(identifier));
                progress.borrow().line(&v, &format!("resolve {reference} done"));
                let base = bases
                    .resolved
                    .borrow()
                    .values()
                    .find(|b| reference.starts_with(&b.reference.to_string()))
                    .map(|b| b.layers.clone())
                    .ok_or_else(|| format!("{reference}: not resolved"))?;
                let r = if read.contains(&i) {
                    exec.image(base, run_images.get(&i).copied())
                        .map_err(|e| fail(&v, &e))?
                } else {
                    exec::Ref {
                        fs: std::rc::Rc::new(shards_build::vfs::Fs::new(
                            shards_image::layer::root(),
                            exec::now(),
                        )),
                        layers: base,
                        stack: shards_build::stack::Stack::unknown("a base image not unpacked"),
                        origin: std::rc::Rc::new(builder::Origin::Scratch),
                    }
                };
                progress.borrow().done(&v);
                vec![r]
            }
            OpKind::Source { identifier, attrs } if identifier.starts_with(b"local://") => {
                let v = progress.borrow_mut().start(&name);
                let list = |key: &[u8]| -> Vec<Vec<u8>> {
                    attrs
                        .get(key)
                        .and_then(|j| serde_json::from_slice::<Vec<String>>(j).ok())
                        .map(|l| l.into_iter().map(String::into_bytes).collect())
                        .unwrap_or_default()
                };
                let filters = shards_build::context::Filters {
                    include: list(b"local.includepatterns"),
                    exclude: list(b"local.excludepatterns"),
                    follow: list(b"local.followpaths"),
                };
                let r = exec.context(&context, &filters).map_err(|e| fail(&v, &e))?;
                let (files, bytes) = context_size(&r.fs);
                progress
                    .borrow()
                    .line(&v, &format!("read {files} files, {} done", human_size(bytes)));
                progress.borrow().done(&v);
                vec![r]
            }
            OpKind::File { actions } => {
                let v = progress.borrow_mut().start(&name);
                let outs = exec.file(&inputs, actions, &name).map_err(|e| fail(&v, &e))?;
                progress.borrow().done(&v);
                outs
            }
            OpKind::Merge => {
                let v = (!name.is_empty()).then(|| progress.borrow_mut().start(&name));
                let r = exec.merge(&inputs);
                let r = match (r, &v) {
                    (Ok(r), _) => r,
                    (Err(e), Some(v)) => return Err(fail(v, &e)),
                    (Err(e), None) => return Err(format!("failed to build: failed to solve: {e}")),
                };
                if let Some(v) = &v {
                    progress.borrow().done(v);
                }
                vec![r]
            }
            OpKind::Source { .. } => {
                let what = name.split_once("] ").map_or(name.as_str(), |(_, s)| s);
                let v = progress.borrow_mut().start(&name);
                return Err(fail(
                    &v,
                    &format!("{what}: this source is not supported by shards build yet"),
                ));
            }
            OpKind::Exec {
                process,
                mounts,
                network,
                security,
                secret_env,
                devices,
            } => {
                let v = progress.borrow_mut().start(&name);
                // No CDI devices reach a builder: an optional one is left out, a required
                // one fails as BuildKit fails it without CDI (solver/llbsolver/vertex.go).
                if let Some(d) = devices.iter().find(|d| !d.optional) {
                    let why = format!(
                        "CDI device {:?} is required by step {:?}, but CDI device support is disabled",
                        show(&d.name),
                        name
                    );
                    return Err(fail(&v, &why));
                }
                if builder.is_none() {
                    let guest = match (std::env::var_os("SHARDS_KERNEL"), std::env::var_os("SHARDS_INIT")) {
                        (Some(k), Some(i)) => (PathBuf::from(k), PathBuf::from(i)),
                        _ => {
                            let g = match crate::guest::current(&home).map_err(|e| fail(&v, &e))? {
                                Some(g) => g,
                                None => {
                                    crate::guest::default(&home, &|_| {}, None).map_err(|e| fail(&v, &e))?
                                }
                            };
                            (g.kernel, g.init)
                        }
                    };
                    let boot = builder::Boot {
                        kernel: &guest.0,
                        init: &guest.1,
                        cpus: builder_cpus(),
                        memory_mib: builder_memory_mib(),
                        bases: &run_bases,
                    };
                    builder = Some(builder::Builder::start(&boot).map_err(|e| fail(&v, &e))?);
                }
                let Some(b) = builder.as_mut() else {
                    return Err(fail(&v, "the builder is gone"));
                };
                let op = exec::RunOp {
                    process,
                    mounts,
                    network: *network,
                    security: *security,
                    secret_env,
                    // `--allow` is not served yet: no entitlement is granted, as BuildKit
                    // grants none without it.
                    insecure: false,
                    network_host: false,
                };
                let mut held = Vec::new();
                let r = exec.run(b, &inputs, &op, &name, &mut |_, bytes| {
                    progress.borrow().log(&v, &mut held, bytes)
                });
                progress.borrow().flush(&v, &mut held);
                let r = r.map_err(|e| fail(&v, &e))?;
                progress.borrow().done(&v);
                r
            }
        };
        results.push(outs);
        crate::phase("op");
    }
    // The image: the target's layers, and its stage's base image for the exporter.
    let mut layers: Vec<Layer> = Vec::new();
    let mut base_image: Option<Image> = None;
    let mut target: Option<exec::Ref> = None;
    if let Some(root) = def.root {
        target = results
            .get(root.op)
            .and_then(|outs| outs.get(usize::try_from(root.index).ok()?))
            .cloned();
        layers = target.as_ref().map(|r| r.layers.clone()).unwrap_or_default();
        // Down the first inputs to the stage's FROM.
        let mut at = root.op;
        while let Some(op) = def.ops.get(at) {
            match &op.kind {
                OpKind::Source { identifier, .. } => {
                    if let Some(reference) = identifier.strip_prefix(b"docker-image://") {
                        let reference = show(reference);
                        base_image = bases
                            .resolved
                            .borrow()
                            .values()
                            .find(|b| reference.starts_with(&b.reference.to_string()))
                            .map(|b| b.image.clone());
                    }
                    break;
                }
                _ => match op.inputs.first() {
                    Some(i) => at = i.op,
                    None => break,
                },
            }
        }
    }
    // Every layer is in the store now. The root filesystem is written from the target's
    // snapshot, put in its layers' form, its files read where the build has them; the
    // other snapshots go first. Where that cannot be done exactly, the snapshots, their
    // sources and stages all go before the export stacks the layers again, so the two
    // never hold memory at once.
    drop(results);
    let flat = target.map(|r| exec.flat(r));
    // With SHARDS_TIMING set, which way the export goes, and why, for tests and
    // benchmarks to hold.
    if std::env::var_os("SHARDS_TIMING").is_some() {
        let line = match &flat {
            Some(Ok(_)) => r#"{"from":"snapshot"}"#.to_string(),
            Some(Err(why)) => format!(
                r#"{{"from":"layers","why":{}}}"#,
                serde_json::Value::from(why.as_str())
            ),
            None => r#"{"from":"none"}"#.to_string(),
        };
        let _ = writeln!(std::io::stderr(), "shards-export {line}");
    }
    let mut exec = match flat {
        Some(Ok(_)) => Some(exec),
        _ => {
            drop(exec);
            None
        }
    };
    crate::phase("drop");

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
    match (flat, exec.as_mut()) {
        _ if store_layers.is_empty() => {}
        (Some(Ok(flat)), Some(exec)) => {
            exec.rootfs(flat, &store_layers)?;
            crate::phase("rootfs-from-snapshot");
        }
        _ => {
            store.rootfs(&store_layers, &limits).map_err(|e| e.to_string())?;
            crate::phase("rootfs-from-layers");
        }
    }
    drop(exec);
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
            .tag(&reference.to_string(), &desc, &manifest_digest, &contents)
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
    crate::phase("end");
    Ok(())
}

/// The files a context holds and their bytes, each file once however many names it has.
/// Nothing is copied: the build reads them in place when it writes a layer.
fn context_size(fs: &shards_build::vfs::Fs) -> (u64, u64) {
    let mut seen = std::collections::HashSet::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    for id in 0..fs.tree().len() {
        if let Some(shards_image::erofs::Node {
            kind: shards_image::erofs::Kind::File { size, data },
            ..
        }) = fs.node(id)
            && seen.insert((data.source, data.offset))
        {
            files += 1;
            bytes += size;
        }
    }
    (files, bytes)
}

/// go-units HumanSize: decimal units, four significant digits.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1000.0 && i < UNITS.len() - 1 {
        size /= 1000.0;
        i += 1;
    }
    format!("{}{}", go_g4(size), UNITS.get(i).unwrap_or(&"B"))
}

/// `strconv.FormatFloat(v, 'g', 4, 64)` for 0 <= v < 10000.
fn go_g4(v: f64) -> String {
    let s = format!("{v:.3e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut out = if exp >= 0 {
        let int_len = (exp + 1) as usize;
        let (int, frac) = if digits.len() > int_len {
            (
                digits.get(..int_len).unwrap_or("").to_string(),
                digits.get(int_len..).unwrap_or("").to_string(),
            )
        } else {
            (format!("{digits:0<int_len$}"), String::new())
        };
        if frac.is_empty() {
            int
        } else {
            format!("{int}.{frac}")
        }
    } else {
        format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
    };
    if out.contains('.') {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    out
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
