//! `shards build`: builds an image as `docker build` does (docs/design/architecture.md
//! D33). The command line is buildx's (`shards_cmdline::commands::BUILD`); the plan is
//! BuildKit's (`shards_dockerfile::plan`); the image is written to this user's store as
//! BuildKit's exporter writes it (`shards_dockerfile::export`), so `shards run` boots it.
//! Progress is BuildKit's plain display.
//!
//! Every Dockerfile instruction runs: file operations (COPY, ADD of the context, of
//! archives and of URLs, WORKDIR) here, on in-memory snapshots as BuildKit's backend makes
//! them (`shards_build`), Git repositories by shards' own client (`git`), and RUN steps in
//! a builder microVM (`builder`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use shards_cmdline::buildflags;
use shards_cmdline::commands::BUILD;
use shards_cmdline::flags::{self, Outcome, Parsed};
use shards_dockerfile::export::{self, Layer};
use shards_dockerfile::go::Time;
use shards_dockerfile::image::Image;
use shards_dockerfile::llb::OpKind;
use shards_dockerfile::parser::Dialect;
use shards_dockerfile::plan::{self, EpochSource, Options, Resolved, Resolver};
use shards_dockerfile::platform::{self, Platform};
use shards_image::oci::{self, Descriptor, Document};
use shards_image::platform as image_platform;
use shards_image::reference::{Algorithm, Digest, Reference};
use shards_image::store::{self, Store};
use shards_registry::pull::{self as registry_pull, Event};

mod azblob;
mod builder;
mod cache;
mod compress;
mod domains;
mod estargz;
mod exec;
mod gha;
mod git;
pub(crate) mod http;
#[cfg(unix)]
mod live;
mod multi;
mod output;
mod provenance;
mod remote;
mod s3;
mod sbom;
mod skills;
mod ssh;
mod sshkey;

/// The SSH agents `--ssh` gives a build's steps, by id: sockets forwarded, or keys served.
pub(crate) type Agents = BTreeMap<String, buildflags::Agent<sshkey::Key>>;
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
    let parsed = match flags::parse(&BUILD, PATH, &argv, &buildflags::validate) {
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
    // What a check (`--call=check`) ends with, which says no error of its own.
    let status = std::cell::Cell::new(0u8);
    match run(&parsed, &status) {
        Ok(()) if status.get() != 0 => ExitCode::from(status.get()),
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
    /// On a colour terminal, shards' own display in place of the plain one.
    #[cfg(unix)]
    live: Option<RefCell<live::Live>>,
}

struct Vertex {
    index: usize,
    started: Instant,
}

impl Progress {
    fn say(&self, text: &str) {
        self.say_bytes(text.as_bytes());
    }

    fn say_bytes(&self, bytes: &[u8]) {
        #[cfg(unix)]
        if self.live.is_some() {
            return;
        }
        if !self.quiet {
            let _ = std::io::stderr().write_all(bytes);
        }
    }

    /// A vertex begins: a blank line after the one before, as progressui separates them.
    fn start(&mut self, name: &str) -> Vertex {
        self.next += 1;
        #[cfg(unix)]
        if let Some(live) = &self.live {
            live.borrow_mut().start(name);
        }
        self.say(&format!("\n#{} {name}\n", self.next));
        Vertex {
            index: self.next,
            started: Instant::now(),
        }
    }

    fn line(&self, v: &Vertex, text: &str) {
        #[cfg(unix)]
        if let Some(live) = &self.live {
            live.borrow_mut().line(v.index, text);
        }
        self.say(&format!("#{} {text}\n", v.index));
    }

    fn done(&self, v: &Vertex) {
        #[cfg(unix)]
        if let Some(live) = &self.live {
            live.borrow_mut().done(v.index);
        }
        let secs = v.started.elapsed().as_secs_f64();
        self.say(&format!("#{} DONE {secs:.1}s\n", v.index));
    }

    fn error(&self, v: &Vertex, message: &str) {
        #[cfg(unix)]
        if let Some(live) = &self.live {
            live.borrow_mut().error(v.index, message);
        }
        self.say(&format!("#{} ERROR: {message}\n", v.index));
    }

    /// A vertex the build cache answered, as progressui says it.
    fn cached(&self, v: &Vertex) {
        #[cfg(unix)]
        if let Some(live) = &self.live {
            live.borrow_mut().cached(v.index);
        }
        self.say(&format!("#{} CACHED\n", v.index));
    }

    /// A vertex the build's failure stopped.
    fn canceled(&self, v: &Vertex) {
        #[cfg(unix)]
        if let Some(live) = &self.live {
            live.borrow_mut().canceled(v.index);
            live.borrow_mut().leave();
        }
        self.say(&format!("#{} CANCELED\n", v.index));
    }
}

/// What a RUN step may print, as buildkitd reads BUILDKIT_STEP_LOG_MAX_SIZE and
/// BUILDKIT_STEP_LOG_MAX_SPEED (util/progress/logs): each stream's bytes, and its bytes
/// per second since it began, -1 for no limit.
#[derive(Clone, Copy)]
struct LogLimits {
    size: i64,
    speed: i64,
}

impl LogLimits {
    fn from_env() -> LogLimits {
        // strconv.ParseInt(v, 10, 32): i32's parse reads a sign and digits as it does.
        let read = |key: &str, default: i64| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<i32>().ok())
                .map_or(default, i64::from)
        };
        LogLimits {
            size: read("BUILDKIT_STEP_LOG_MAX_SIZE", 2 << 20),
            speed: read("BUILDKIT_STEP_LOG_MAX_SPEED", 200 << 10),
        }
    }
}

/// The last bytes of a stream, once it is clipped, for its end: armon/circbuf's Buffer
/// of 256 KiB, as buildkitd keeps one.
struct Tail {
    data: Vec<u8>,
    at: usize,
    full: bool,
}

impl Tail {
    const SIZE: usize = 256 << 10;

    fn write(&mut self, mut bytes: &[u8]) {
        if bytes.len() > Self::SIZE {
            bytes = bytes.get(bytes.len() - Self::SIZE..).unwrap_or_default();
        }
        while !bytes.is_empty() {
            let room = Self::SIZE - self.at;
            let n = room.min(bytes.len());
            if let (Some(to), Some(from)) = (self.data.get_mut(self.at..self.at + n), bytes.get(..n)) {
                to.copy_from_slice(from);
            }
            self.at = (self.at + n) % Self::SIZE;
            self.full |= self.at == 0;
            bytes = bytes.get(n..).unwrap_or_default();
        }
    }

    fn bytes(&self) -> Vec<u8> {
        if self.full {
            [go_tail(&self.data, self.at), go_head(&self.data, self.at)].concat()
        } else {
            go_head(&self.data, self.at).to_vec()
        }
    }
}

fn go_tail(b: &[u8], at: usize) -> &[u8] {
    b.get(at..).unwrap_or_default()
}

fn go_head(b: &[u8], at: usize) -> &[u8] {
    b.get(..at).unwrap_or_default()
}

/// One stream of a step as buildkitd passes it on: its streamWriter.
struct Clip {
    began: Instant,
    size: i64,
    clipping: bool,
    by_speed: bool,
    tail: Option<Tail>,
}

impl Clip {
    fn new() -> Clip {
        Clip {
            began: Instant::now(),
            size: 0,
            clipping: false,
            by_speed: false,
            tail: None,
        }
    }

    /// `checkLimit`: how much of `n` bytes more may pass.
    fn room(&mut self, n: usize, limits: LogLimits) -> usize {
        let old = self.size;
        self.size = self.size.saturating_add(i64::try_from(n).unwrap_or(i64::MAX));
        let mut max = -1;
        if limits.speed != -1 {
            // A second begun counts whole.
            let secs = self.began.elapsed().as_secs_f64().ceil();
            max = (secs as i64).saturating_mul(limits.speed);
            self.by_speed = true;
        }
        if max == -1 || max > limits.size {
            max = limits.size;
            self.by_speed = false;
        }
        if max != -1 {
            if max < old {
                return 0;
            }
            if self.size > max {
                return usize::try_from(max - old).unwrap_or(0);
            }
        }
        n
    }

    /// `Write`: what of `bytes` passes, a notice where clipping begins; all of it kept in
    /// the tail once a write is clipped.
    fn write(&mut self, bytes: &[u8], limits: LogLimits) -> Vec<u8> {
        let room = self.room(bytes.len(), limits);
        if self.tail.is_none() && room < bytes.len() {
            self.tail = Some(Tail {
                data: vec![0; Tail::SIZE],
                at: 0,
                full: false,
            });
        }
        if let Some(tail) = &mut self.tail {
            tail.write(bytes);
        }
        let mut out = bytes.get(..room).unwrap_or_default().to_vec();
        if self.clipping && room == bytes.len() {
            self.clipping = false;
        }
        if !self.clipping && room != bytes.len() {
            let limit = if self.by_speed {
                format!("{}/s", units(limits.speed))
            } else {
                units(limits.size)
            };
            out.extend_from_slice(format!("\n[output clipped, log limit {limit} reached]\n").as_bytes());
            self.clipping = true;
        }
        out
    }
}

/// tonistiigi/units' `%#g` of a byte count: whole bytes below a KiB ("1000B"), else its
/// value in the largest binary unit it reaches, as Go writes a float64 shortest.
fn units(b: i64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut i = 0;
    let mut base: i64 = 1;
    while i + 1 < UNITS.len() && b.unsigned_abs() >= base.unsigned_abs().saturating_mul(1024) {
        base = base.saturating_mul(1024);
        i += 1;
    }
    match UNITS.get(i) {
        Some(&unit) if i > 0 => format!("{}{unit}", b as f64 / base as f64),
        _ => format!("{b}B"),
    }
}

/// A RUN step's output as buildkitd clips it, each stream on its own, and progressui's
/// plain mode prints it.
struct StepLog {
    limits: LogLimits,
    streams: [Clip; 2],
    /// Whether the last line printed has not ended, so that what comes next continues it.
    partial: bool,
}

impl StepLog {
    fn new(limits: LogLimits) -> StepLog {
        StepLog {
            limits,
            streams: [Clip::new(), Clip::new()],
            partial: false,
        }
    }

    /// A chunk of stream `which`.
    fn write(&mut self, p: &Progress, v: &Vertex, which: u8, bytes: &[u8]) {
        let stream = usize::from(which == shards_abi::run::kind::STDERR);
        let limits = self.limits;
        let Some(clip) = self.streams.get_mut(stream) else {
            return;
        };
        let out = clip.write(bytes, limits);
        #[cfg(unix)]
        if let Some(live) = &p.live {
            live.borrow_mut().output(v.index, &out);
            return;
        }
        p.say_bytes(&self.show(v, &out));
    }

    /// progressui: each line stamped with the seconds since the step began when its first
    /// bytes came, to 3 places below 10 s, 2 below 100 and 1 past; a line not yet ended
    /// printed as far as it goes, and the rest after it as it comes.
    fn show(&mut self, v: &Vertex, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let secs = v.started.elapsed().as_secs_f64();
        let places = if secs < 10.0 {
            3
        } else if secs < 100.0 {
            2
        } else {
            1
        };
        let stamp = format!("#{} {secs:.places$} ", v.index);
        let mut rest = data;
        let mut first = true;
        // `split`: no line from no bytes, and none ended.
        let complete = loop {
            if rest.is_empty() {
                break !data.is_empty();
            }
            let end = rest.iter().position(|&b| b == b'\n');
            if !(first && self.partial) {
                out.extend_from_slice(stamp.as_bytes());
            }
            out.extend_from_slice(go_head(rest, end.unwrap_or(rest.len())));
            let Some(end) = end else {
                break false;
            };
            out.push(b'\n');
            rest = go_tail(rest, end + 1);
            first = false;
        };
        self.partial = !complete;
        out
    }

    /// The step's end: each stream's tail, stdout's first, as flushBuffer passes them
    /// on unclipped, and a line left open ended, as the printer ends it.
    fn end(&mut self, p: &Progress, v: &Vertex) {
        for i in 0..2 {
            let tail = self
                .streams
                .get_mut(i)
                .and_then(|s| s.tail.take())
                .map(|t| t.bytes());
            if let Some(tail) = tail {
                p.say_bytes(&self.show(v, &tail));
            }
        }
        if self.partial {
            p.say("\n");
            self.partial = false;
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
pub(crate) fn host_memory() -> Option<u64> {
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
pub(crate) fn host_memory() -> Option<u64> {
    None
}

/// A base image as the build resolved it.
#[derive(Debug, Clone)]
struct Base {
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
    /// Each base, by what the planner names its source and the platform it was resolved
    /// for ([`base_key`]): the name it resolved, without any digest it carried, then `@`
    /// and the digest it resolved to, which an index's platforms share.
    resolved: RefCell<BTreeMap<String, Base>>,
    /// The manifests of OCI layouts named contexts name, imported into the store: by the
    /// digest the context names, the manifest for this platform.
    layouts: BTreeMap<String, Descriptor>,
    /// The OSI artifacts the build takes, by what the planner names their sources: their
    /// content layers, read as image layers.
    artifacts: RefCell<BTreeMap<String, Vec<Layer>>>,
    /// The build's secrets and SSH agents, for a Git source SOURCE_DATE_EPOCH names.
    secrets: &'a BTreeMap<String, buildflags::SecretBytes>,
    agents: &'a Agents,
    /// What each name resolved to for each platform: asked again, the same, and no step
    /// of its own, as BuildKit's resolve is one vertex for its identifier and platform
    /// (llbsolver/bridge.go resolveSourceMetadata).
    answered: RefCell<Answered>,
}

/// Base images as resolved, by name and platform.
type Answered = BTreeMap<(Vec<u8>, Vec<u8>), Resolved>;

impl Resolver for Bases<'_> {
    fn resolve(&self, name: &[u8], platform: &Platform, log: &[u8]) -> Result<Resolved, Vec<u8>> {
        let key = (name.to_vec(), platform::format_all(platform));
        if let Some(r) = self.answered.borrow().get(&key) {
            return Ok(r.clone());
        }
        let name = String::from_utf8_lossy(name).into_owned();
        let v = self.progress.borrow_mut().start(&String::from_utf8_lossy(log));
        let r = self.fetch(&name, platform);
        let progress = self.progress.borrow();
        match &r {
            Ok(resolved) => {
                progress.done(&v);
                self.answered.borrow_mut().insert(key, resolved.clone());
            }
            Err(e) => progress.error(&v, &String::from_utf8_lossy(e)),
        }
        r
    }

    /// An OSI artifact, from the store or pulled (with `--pull`, pulled again), checked:
    /// its type, config and content (crate::agent::fetch); its content layers kept for the
    /// source that names it.
    fn artifact(&self, name: &[u8], kind: &[u8], log: &[u8]) -> Result<Resolved, Vec<u8>> {
        let v = self.progress.borrow_mut().start(&String::from_utf8_lossy(log));
        let r = self.artifact_of(&String::from_utf8_lossy(name), kind);
        let progress = self.progress.borrow();
        match &r {
            Ok(_) => progress.done(&v),
            Err(e) => progress.error(&v, e),
        }
        r.map_err(String::into_bytes)
    }

    /// Each under the name of the step BuildKit's metadata resolution shows
    /// (dockerfile/1.27.1 epoch.go).
    fn epoch(&self, source: &EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>> {
        let name = match source {
            EpochSource::Context => "[internal] resolve main build context metadata".to_string(),
            EpochSource::Http { stage, .. } | EpochSource::Git { stage, .. } => {
                format!(
                    "[internal] resolve SOURCE_DATE_EPOCH source stage {}",
                    show(stage)
                )
            }
        };
        let v = self.progress.borrow_mut().start(&name);
        let r = self.source_time(source);
        let progress = self.progress.borrow();
        match &r {
            Ok(_) => progress.done(&v),
            Err(e) => progress.error(&v, e),
        }
        r.map_err(String::into_bytes)
    }
}

impl Bases<'_> {
    fn artifact_of(&self, name: &str, kind: &[u8]) -> Result<Resolved, String> {
        let want = match kind {
            b"agent" => shards_image::osi::Kind::Agent,
            b"harness" => shards_image::osi::Kind::Harness,
            _ => shards_image::osi::Kind::Mcp,
        };
        let reference = Reference::parse(name).map_err(|e| format!("{name}: {e}"))?;
        let (desc, manifest, _) = crate::agent::fetch(self.store, &reference, want, self.pull, &|_| {})?;
        let diff_ids = crate::agent::diff_ids(self.store, want, &manifest)?;
        let mut layers = Vec::new();
        for (l, diff_id) in manifest.layers.iter().zip(diff_ids) {
            let media = crate::agent::layer_type(want, &l.media_type)
                .ok_or_else(|| format!("{name}: a layer of type {}", l.media_type))?;
            layers.push(Layer {
                media_type: media.as_bytes().to_vec(),
                digest: l.digest.as_bytes().to_vec(),
                size: u64::try_from(l.size).map_err(|_| format!("{name}: a layer's size is negative"))?,
                diff_id: diff_id.to_string().into_bytes(),
                annotations: BTreeMap::new(),
                created: None,
                description: format!("the {} {reference}", want.word()).into_bytes(),
            });
        }
        let mut bare = reference.clone();
        bare.digest = Some(desc.digest().map_err(|e| e.to_string())?);
        let resolved = bare.to_string();
        self.artifacts.borrow_mut().insert(resolved.clone(), layers);
        let config = self
            .store
            .content(&manifest.config, oci::MAX_CONFIG)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{name}: its config is not here"))?;
        Ok(Resolved {
            reference: resolved.into_bytes(),
            digest: Some(desc.digest.clone().into_bytes()),
            config,
        })
    }

    /// When `source` says it was made (resolveSourceDateEpochFromState): a local context
    /// says nothing, BuildKit's metadata of one being neither Git's nor HTTP's; a URL
    /// fetched without a checksum, its Last-Modified if it has one; else the newest
    /// regular file of what it fetches, if that is an archive.
    fn source_time(&self, source: &EpochSource) -> Result<Option<(i64, u32)>, String> {
        match source {
            EpochSource::Context => Ok(None),
            EpochSource::Http { url, checksum, .. } => {
                let limits = crate::pull::limits()?;
                // Removed, with what it holds, once the time is known.
                let stage = self.store.stage().map_err(|e| e.to_string())?;
                let checksum = checksum.as_deref().map(show);
                let fetched = http::fetch_now(
                    &show(url),
                    checksum.as_deref(),
                    stage.path().join("source"),
                    &limits,
                )?;
                if checksum.is_none()
                    && let Some(t) = fetched.last_modified
                {
                    return Ok(Some(t));
                }
                let file = std::fs::File::open(&fetched.path).map_err(|e| e.to_string())?;
                shards_build::archive::newest_file(file, &limits).map_err(|e| e.0)
            }
            EpochSource::Git { git: g, .. } => {
                let (identifier, attrs) = plan::git_identifier(g, false, &g.checksum);
                let src = git::source(&identifier, &attrs)?;
                let bound = usize::try_from(crate::pull::limits()?.bytes).unwrap_or(usize::MAX);
                let limits = shards_git::remote::Limits {
                    pack: bound,
                    object: bound,
                };
                let auth = git::auth(&src, self.secrets);
                git::commit_time(
                    &src,
                    limits,
                    &shards_registry::http::Cancel::new(),
                    auth.as_ref(),
                    self.agents,
                )
                .map(Some)
            }
        }
    }
}

impl Bases<'_> {
    fn fetch(&self, name: &str, wanted: &Platform) -> Result<Resolved, Vec<u8>> {
        let fail = |e: String| e.into_bytes();
        let reference = Reference::parse(name).map_err(|e| fail(e.to_string()))?;
        // An OCI layout's image, imported already: no registry is asked.
        if let Some(d) = &reference.digest
            && let Some(record) = self.layouts.get(&d.to_string())
        {
            return self.base_of(name, &reference, record.clone(), d.clone(), wanted);
        }
        if !ours(wanted) {
            return self.foreign(name, &reference, wanted);
        }
        let limits = crate::pull::limits().map_err(fail)?;
        // Pinned by digest: the image the store holds of that digest, whatever it is named
        // here, the digest as asked.
        let held = match &reference.digest {
            Some(d) if !self.pull => self.store.holding(d).map_err(|e| fail(e.to_string()))?,
            _ => None,
        };
        if let (Some(held), Some(d)) = (&held, &reference.digest)
            && let Some(record) = self.store.tagged(held).map_err(|e| fail(e.to_string()))?
        {
            registry_pull::local_tagged(
                self.store,
                held,
                &reference.familiar(),
                &image_platform::guest(),
                &limits,
            )
            .map_err(|e| fail(e.to_string()))?;
            return self.base_of(name, &reference, record, d.clone(), wanted);
        }
        let local = if self.pull {
            None
        } else {
            registry_pull::local(self.store, &reference, &image_platform::guest(), &limits)
                .map_err(|e| fail(e.to_string()))?
        };
        let pulled = match local {
            Some(p) => p,
            None => {
                crate::pull::fetch(
                    self.home,
                    &reference,
                    &image_platform::guest(),
                    &|_: Event<'_>| {},
                    &|_| {},
                    None,
                    &|k| std::env::var(k).ok(),
                    false,
                )
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
        self.base_of(name, &reference, record, pulled.resolved.clone(), wanted)
    }

    /// A base for a platform this host's microVMs do not run: its manifest for that
    /// platform, config and layers fetched into the store with no name recorded, so that
    /// the image a name holds stays the guests' (BuildKit's pull leaves an image store's
    /// names be), and kept as the build cache keeps what it made, until `builder prune`.
    /// Found there again unless `--pull`, as a guest's base is found in the store.
    fn foreign(&self, name: &str, reference: &Reference, wanted: &Platform) -> Result<Resolved, Vec<u8>> {
        let fail = |e: String| e.into_bytes();
        let target = image_platform::normalize(&oci::Platform {
            os: show(&wanted.os),
            architecture: show(&wanted.architecture),
            variant: (!wanted.variant.is_empty()).then(|| show(&wanted.variant)),
            os_features: Vec::new(),
        });
        let key = sha256(format!("foreign base\0{reference}\0{}", base_platform(wanted)).as_bytes())
            .hex()
            .to_string();
        if !self.pull
            && let Some(body) = self.store.cache_get(&key).map_err(|e| fail(e.to_string()))?
            && let Ok(held) = serde_json::from_slice::<ForeignBase>(&body)
            && let Ok(resolved) = Digest::parse(&held.resolved)
        {
            self.store.cache_used(&key).map_err(|e| fail(e.to_string()))?;
            return self.base_of(name, reference, held.manifest, resolved, wanted);
        }
        let limits = crate::pull::limits().map_err(fail)?;
        let registry = crate::pull::registry(reference, None, &|k| std::env::var(k).ok()).map_err(fail)?;
        let (resolved, manifest) =
            shards_registry::pull::fetch_untagged(&registry, self.store, reference, &[target], &limits)
                .map_err(|e| fail(e.to_string()))?;
        let out = self.base_of(name, reference, manifest.clone(), resolved.clone(), wanted)?;
        // Its blobs, kept while the record is: the manifest, the config, the layers.
        let bytes = self
            .store
            .content(&manifest, oci::MAX_MANIFEST)
            .map_err(|e| fail(e.to_string()))?
            .ok_or_else(|| fail(format!("{name}: its manifest is missing")))?;
        let Document::Manifest(m) =
            oci::parse_document(&bytes, &manifest.media_type).map_err(|e| fail(e.to_string()))?
        else {
            return Err(fail(format!("{name}: its manifest is an index")));
        };
        let mut blobs = vec![manifest.digest().map_err(|e| fail(e.to_string()))?];
        let mut size = u64::try_from(manifest.size).unwrap_or(0);
        for d in std::iter::once(&m.config).chain(&m.layers) {
            blobs.push(d.digest().map_err(|e| fail(e.to_string()))?);
            size = size.saturating_add(u64::try_from(d.size).unwrap_or(0));
        }
        let held = ForeignBase {
            resolved: resolved.to_string(),
            manifest,
        };
        let body = serde_json::to_string(&held).map_err(|e| fail(e.to_string()))?;
        self.store
            .cache_put(&key, &blobs, size, &body)
            .map_err(|e| fail(e.to_string()))?;
        Ok(out)
    }

    /// The base `record` names, a manifest the store holds, as `name` resolved to
    /// `resolved`.
    fn base_of(
        &self,
        name: &str,
        reference: &Reference,
        record: Descriptor,
        resolved: Digest,
        platform: &Platform,
    ) -> Result<Resolved, Vec<u8>> {
        let fail = |e: String| e.into_bytes();
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
        let out = Resolved {
            reference: reference.to_string().into_bytes(),
            digest: Some(resolved.to_string().into_bytes()),
            config,
        };
        let bare = name.rsplit_once('@').map_or(name, |(bare, _)| bare);
        let base = Base { image, layers };
        // A named context's image is its source by the reference alone, as BuildKit's
        // NamedContext names it (no digest).
        if !name.contains('@') {
            self.resolved
                .borrow_mut()
                .insert(base_key(name, platform), base.clone());
        }
        self.resolved
            .borrow_mut()
            .insert(base_key(&format!("{bare}@{resolved}"), platform), base);
        Ok(out)
    }
}

/// A foreign base as the build cache keeps it: what its name resolved to, and the
/// manifest chosen for its platform.
#[derive(serde::Serialize, serde::Deserialize)]
struct ForeignBase {
    resolved: String,
    manifest: Descriptor,
}

/// Whether this host's microVMs run `p`: its OS and architecture are theirs.
fn ours(p: &Platform) -> bool {
    let host = host_platform();
    p.os == host.os && p.architecture == host.architecture
}

/// A platform as a base's key names it: in full, variant and all.
fn base_platform(p: &Platform) -> String {
    show(&platform::format_all(p))
}

/// The key [`Bases::resolved`] keeps a base under: the source's reference, and the
/// platform it was resolved for, which an index's platforms share the digest of.
fn base_key(reference: &str, p: &Platform) -> String {
    format!("{reference} {}", base_platform(p))
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

/// An HTTP source's identifier: its URL.
fn is_http(identifier: &[u8]) -> bool {
    identifier.starts_with(b"https://") || identifier.starts_with(b"http://")
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The platforms `--platform` names, as buildx reads them (util/platformutil Parse): each
/// value a comma-separated list, each normalized, and the same platform once, as
/// `Dedupe` keeps it, where BuildKit would build and export it twice. `local` is the
/// platform this host's microVMs run, where buildx takes its client's own, which on a
/// Mac is no platform a Linux build makes.
fn target_platforms(given: &[String], host: &Platform) -> Result<Vec<Platform>, String> {
    let mut out: Vec<Platform> = Vec::new();
    for value in given {
        for one in value.split(',') {
            let one = one.trim();
            let p = if one.eq_ignore_ascii_case("local") {
                host.clone()
            } else {
                platform::normalize(&platform::parse(one.as_bytes(), host).map_err(|e| show(&e))?)
            };
            if !out
                .iter()
                .any(|o| platform::format_all(o) == platform::format_all(&p))
            {
                out.push(p);
            }
        }
    }
    Ok(out)
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

/// dockerui's ReadFile: what `reader` holds, all of it. BuildKit's frontend refuses a
/// Dockerfile or ignore file past 16 MiB, containerd's DefaultMaxRecvMsgSize, the largest
/// message its gRPC takes from the client (dockerfile/1.27.1 frontend/dockerui/readfile.go):
/// a limit of its transport, which shards, reading the file where it is, does not have.
fn read_whole(mut reader: impl Read, name: &str) -> Result<Vec<u8>, String> {
    let mut text = Vec::new();
    reader
        .read_to_end(&mut text)
        .map_err(|e| format!("{name}: {e}"))?;
    Ok(text)
}

/// A file's bytes, or none if it is not there, read as [`read_whole`] reads, `name`
/// being what its errors call it. Unlike BuildKit, which has the client send the
/// Dockerfile and ignore files over a session, shards reads them where they are.
fn read_if_present(path: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    match std::fs::File::open(path) {
        Ok(file) => read_whole(file, name).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A Dockerfile's name, its text, and the ignore file beside it.
type Dockerfile = (String, Vec<u8>, Option<Vec<u8>>);

/// What a file named `name` is read as: an Agentfile where it is named as one, as Docker's
/// names a Dockerfile (`Agentfile`, `*.Agentfile`, `Agentfile.*`), else a Dockerfile (D35).
fn dialect_of(name: &str) -> Dialect {
    let lower = name.to_ascii_lowercase();
    if lower == "agentfile" || lower.ends_with(".agentfile") || lower.starts_with("agentfile.") {
        Dialect::Agentfile
    } else {
        Dialect::Dockerfile
    }
}

/// The file a build reads: `-f`'s path, or none for stdin's; without `-f`, the context's
/// `Agentfile` where there is one, else its `Dockerfile` (D35). Its name as the build's
/// progress shows it.
fn definition(parsed: &Parsed, context: &Path) -> (Option<PathBuf>, String) {
    let file = parsed.string("file");
    if file == "-" {
        return (None, "Dockerfile".into());
    }
    let path = if !file.is_empty() {
        PathBuf::from(file)
    } else if let Some(agentfile) = ["Agentfile", "agentfile"]
        .iter()
        .map(|n| context.join(n))
        .find(|p| p.is_file())
    {
        agentfile
    } else {
        context.join("Dockerfile")
    };
    let name = path
        .file_name()
        .map_or_else(|| "Dockerfile".into(), |n| n.to_string_lossy().into_owned());
    (Some(path), name)
}

/// The Dockerfile, found as the frontend finds it (dockerui's Client.ReadEntrypoint):
/// `-f`'s file, stdin's, or PATH's `Dockerfile`, then `dockerfile` for the default name.
/// Its name, text, and the `<name>.dockerignore` beside it, which overrides the
/// context's `.dockerignore`. An Agentfile is found before them (`definition`).
fn dockerfile(parsed: &Parsed, context: &Path) -> Result<Dockerfile, String> {
    let read_failed = |e: String| format!("failed to read dockerfile: {e}");
    let (path, name) = definition(parsed, context);
    let Some(path) = path else {
        let text = read_whole(std::io::stdin(), "Dockerfile").map_err(read_failed)?;
        return Ok(("Dockerfile".into(), text, None));
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    // One too large is no reason to look for `dockerfile` instead.
    let mut text = read_if_present(&path, &name).map_err(read_failed)?;
    if text.is_none() && name == "Dockerfile" {
        text = read_if_present(&dir.join("dockerfile"), "dockerfile").map_err(read_failed)?;
    }
    let Some(text) = text else {
        return Err(format!(
            "failed to read dockerfile: open {name}: no such file or directory"
        ));
    };
    let beside_name = format!("{name}.dockerignore");
    let beside = read_if_present(&dir.join(&beside_name), &beside_name)?;
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

fn run(parsed: &Parsed, status: &std::cell::Cell<u8>) -> Result<(), String> {
    crate::phase("start");
    // When the build began, as its provenance says it (D71).
    let started = unix_now();
    // What the build is given, in the order buildx's runBuild meets it: its secrets, read
    // now and once, each as large as a step carries; then its entitlements; its ulimits
    // were read with its flags (shards_cmdline::buildflags).
    // Its outputs as buildx reads them, first (toOptions); checked once its agents are.
    let familiar = |n: &str| {
        Reference::parse_normalized(n)
            .map(|r| r.familiar())
            .map_err(|e| e.to_string())
    };
    // The attestations asked for, read as buildx reads them, before the contexts.
    let (provenance_asked, sbom_asked) = provenance_of(parsed)?;
    let named = buildflags::parse_contexts(parsed.many("build-context"), &familiar)?;
    let exports = buildflags::parse_exports(parsed.many("output"))?;
    buildflags::check_iidfile(&exports, parsed.string("iidfile"))?;
    let env = |name: &str| std::env::var_os(name).map(os_bytes);
    let secrets = buildflags::store(
        buildflags::parse_secrets(parsed.many("secret"))?,
        &env,
        u64::from(shards_abi::run::MAX_PAYLOAD),
    )?;
    // --builder, or BUILDX_BUILDER: shards' own builder, or Docker's default context's,
    // which here is the same one; any other, as buildx refuses a name it does not know
    // (storeutil.GetNodeGroup).
    let builder_name = match parsed.string("builder") {
        "" => std::env::var("BUILDX_BUILDER").unwrap_or_default(),
        b => b.to_string(),
    };
    if !matches!(builder_name.as_str(), "" | "shards" | "default") {
        return Err(format!(
            "no builder {} found",
            shards_cmdline::go::quote(&builder_name)
        ));
    }
    let mut allowed = buildflags::parse_entitlements(parsed.many("allow"))?;
    // The flags buildx takes and BuildKit ignores, some said so (build.go checkWarnedFlags,
    // logrus as buildx formats it).
    for (flag, said) in [
        ("isolation", "isolation flag is deprecated with BuildKit."),
        (
            "security-opt",
            "security-opt flag is deprecated. \"RUN --security=insecure\" should be used with BuildKit.",
        ),
        (
            "squash",
            "experimental flag squash is removed with BuildKit. You should squash inside build using a multi-stage Dockerfile for efficiency.",
        ),
    ] {
        if parsed.changed(flag) {
            let _ = writeln!(std::io::stderr(), "WARNING: {said}");
        }
    }
    // --network: the frontend's force-network-mode; host grants itself (build/opt.go).
    let network_mode = match parsed.string("network") {
        "host" => {
            if !allowed.grants(buildflags::NETWORK_HOST) {
                allowed.granted.push(buildflags::NETWORK_HOST.to_string());
            }
            shards_dockerfile::llb::NetMode::Host
        }
        "none" => shards_dockerfile::llb::NetMode::None,
        "" | "default" => shards_dockerfile::llb::NetMode::Sandbox,
        other => {
            return Err(format!(
                "network mode {} not supported by buildkit - you can define a custom network for your builder using the network driver-opt in buildx create",
                shards_cmdline::go::quote(other)
            ));
        }
    };
    // --add-host: host-gateway is the builder's gateway, on the bridge it is elected.
    let gateway = || -> Result<String, String> {
        #[cfg(unix)]
        {
            shards_net::bridge::elected_here(&mut |_| {})
                .map(|b| b.gateway().to_string())
                .ok_or_else(|| shards_net::bridge::NO_SUBNET.to_string())
        }
        #[cfg(not(unix))]
        {
            Err("the builder has no network on this platform yet".to_string())
        }
    };
    let add_hosts = buildflags::add_hosts(parsed.many("add-host"), &gateway)?;
    // --resource, after the legacy flags as buildx gathers them.
    let mut resources: Vec<String> = [
        ("memory", "memory"),
        ("memory-swap", "memory-swap"),
        ("cpu-shares", "cpu-shares"),
        ("cpu-period", "cpu-period"),
        ("cpu-quota", "cpu-quota"),
        ("cpuset-cpus", "cpuset-cpus"),
        ("cpuset-mems", "cpuset-mems"),
    ]
    .iter()
    .filter(|(flag, _)| parsed.changed(flag))
    .map(|(flag, key)| format!("{key}={}", parsed.string(flag)))
    .collect();
    resources.extend(parsed.many("resource").iter().cloned());
    let resource_attrs = buildflags::resource_attrs(&resources)?;
    // The platforms, read as buildx reads them after the resource limits.
    let target_platforms = target_platforms(parsed.many("platform"), &host_platform())?;
    let target_platform = target_platforms.first().cloned().unwrap_or_else(host_platform);
    let mut agents: Agents = buildflags::ssh_agents(
        &buildflags::parse_ssh(parsed.many("ssh")),
        &|k| std::env::var(k).ok(),
        &sshkey::parse,
    )?;
    for agent in agents.values_mut() {
        if let buildflags::Agent::Keys(keys) = agent {
            *keys = sshkey::keyring(std::mem::take(keys));
        }
    }
    let outputs = buildflags::create_exports(
        &exports,
        parsed.bool("push"),
        parsed.bool("load"),
        allowed.local_delete,
        &|p| match std::fs::metadata(p) {
            Ok(m) if m.is_dir() => Ok(buildflags::Found::Dir),
            Ok(_) => Ok(buildflags::Found::File),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(buildflags::Found::Nothing),
            Err(e) => Err(e.to_string()),
        },
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        &safe_delete_dest,
    )
    .map_err(|e| format!("failed to build: {e}"))?;
    check_outputs(&outputs)?;
    let (compression, rewrite_timestamp) = compression_of(&outputs)?;
    // A provenance asked for (not inline-only) goes in every output, which shards makes
    // in an image it stores or pushes alone yet.
    if let Provenance::Explicit {
        inline_only: false, ..
    } = provenance_asked
        && let Some(o) = outputs
            .iter()
            .find(|o| matches!(o.kind.as_str(), "docker" | "tar"))
    {
        return Err(format!(
            "a provenance attestation in a {} output is not supported by shards yet",
            o.kind
        ));
    }
    let (manifest_annotations, descriptor_annotations) = annotations_of(parsed, &outputs, &target_platform)?;
    // toSolveOpt: an image pushed must have a name.
    let pushes = outputs.iter().any(|o| {
        o.kind == "image"
            && o.attrs
                .get("push")
                .is_some_and(|p| shards_cmdline::go::parse_bool(p).unwrap_or(false))
    });
    if pushes && parsed.many("tag").is_empty() {
        return Err("failed to build: tag is needed when pushing to registry".into());
    }
    let ulimits: Vec<shards_dockerfile::llb::Ulimit> = buildflags::ulimits(parsed.many("ulimit"))?
        .into_iter()
        .map(|u| shards_dockerfile::llb::Ulimit {
            name: u.name.into_bytes(),
            soft: u.soft,
            hard: u.hard,
        })
        .collect();
    // The external caches (D62), read as buildx reads them; BUILDKIT_INLINE_CACHE, a
    // true build argument, asks for an inline one (build/opt.go).
    let env = |k: &str| std::env::var(k).ok();
    let cache_from = buildflags::cache_entries(parsed.many("cache-from"), &env)?;
    let mut cache_to = buildflags::cache_entries(parsed.many("cache-to"), &env)?;
    if build_args(parsed.many("build-arg"), true)
        .get(b"BUILDKIT_INLINE_CACHE".as_slice())
        .is_some_and(|v| shards_cmdline::go::parse_bool(&show(v)).unwrap_or(false))
    {
        cache_to.push(buildflags::CacheEntry {
            kind: "inline".into(),
            attrs: BTreeMap::new(),
        });
    }
    remote::check(&cache_to, &env)?;
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
    let asked = parsed.string("progress");
    let mode = match asked {
        "auto" | "plain" | "tty" => "plain",
        "quiet" | "none" => "quiet",
        "rawjson" => return Err("--progress=rawjson is not supported by shards yet".into()),
        other => return Err(format!("invalid progress mode {other:?}")),
    };
    let quiet = parsed.bool("quiet") || mode == "quiet";
    let host = host_platform();
    let progress = RefCell::new(Progress {
        quiet,
        // A platform's build of several numbers on from the one before it.
        next: multi::with(|s| s.next).unwrap_or(0),
        // `auto` or `tty` on a colour terminal: shards' own display.
        #[cfg(unix)]
        live: if !quiet && matches!(asked, "auto" | "tty") {
            live::Live::new().map(RefCell::new)
        } else {
            None
        },
    });
    // Said once of a build of several platforms, not again by each platform's.
    if !multi::active() {
        progress
            .borrow()
            .say("#0 building with \"shards\" instance using shards driver\n");
    }

    let (name, text, beside) = {
        let shown = definition(parsed, &context).1;
        // A platform's build of several reads it again, said once by the build of them.
        if multi::active() {
            let (name, text, ignore) =
                dockerfile(parsed, &context).map_err(|e| format!("failed to build: failed to solve: {e}"))?;
            (name, text, ignore)
        } else {
            let v = progress
                .borrow_mut()
                .start(&format!("[internal] load build definition from {shown}"));
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
        }
    };

    // The context's .dockerignore, unless one is beside the Dockerfile; its vertex shows
    // after the metadata, as the frontend reads it once the stages are planned.
    let context_ignore = match &beside {
        Some(_) => None,
        None => Some(
            read_if_present(&context.join(".dockerignore"), ".dockerignore").map_err(|e| {
                format!("failed to build: failed to solve: failed to read dockerignore patterns: {e}")
            })?,
        ),
    };
    // `<Dockerfile>.dockerignore` beside the Dockerfile, else the context's.
    let ignore_text = beside
        .as_ref()
        .or(context_ignore.as_ref().and_then(Option::as_ref));
    let excludes = ignore_text
        .map(|t| shards_dockerfile::ignore::read_all(t))
        .unwrap_or_default();

    let named = named_contexts(&named)?;
    let home = shards_ipc::home()?;
    let store = crate::pull::store(&home)?;
    let _lease = store.lease().map_err(|e| e.to_string())?;
    let imported = remote::Imported::read(&cache_from, &store, &progress, &env)?;
    let bases = Bases {
        home: &home,
        store: &store,
        pull: parsed.bool("pull"),
        progress: &progress,
        resolved: RefCell::new(multi::with(|sub| std::mem::take(&mut sub.bases)).unwrap_or_default()),
        answered: RefCell::new(multi::with(|sub| std::mem::take(&mut sub.answered)).unwrap_or_default()),
        artifacts: RefCell::new(BTreeMap::new()),
        secrets: &secrets,
        agents: &agents,
        layouts: named
            .layouts
            .iter()
            .map(|(dir, digest)| import_layout(&store, dir, digest).map(|d| (digest.to_string(), d)))
            .collect::<Result<_, String>>()?,
    };
    // What the build's provenance records of its request, as buildx sends it (D71).
    let filename = Path::new(&name)
        .file_name()
        .map_or_else(|| "Dockerfile".to_string(), |n| n.to_string_lossy().into_owned());
    let request_attrs = buildx_attrs(parsed, &filename, &add_hosts, &resource_attrs, &named)?;
    let mut request_locals: Vec<String> = vec!["context".into(), "dockerfile".into()];
    request_locals.extend(named.locals.keys().map(|k| show(k)));
    request_locals.sort();
    let opts = Options {
        target_platform: target_platform.clone(),
        build_platforms: vec![host],
        build_args: build_args(parsed.many("build-arg"), true),
        target: parsed.string("target").as_bytes().to_vec(),
        labels: build_args(parsed.many("label"), false),
        hostname: Vec::new(),
        ulimits,
        // One platform's build of several: its steps named with its platform.
        multi_platform: multi::active(),
        context_id: format!("shards-{}", std::process::id()).into_bytes(),
        excludes,
        dialect: dialect_of(&name),
        all_stages: false,
        contexts: named.contexts,
        context_keys: named.keys,
        context_excludes: named.excludes,
        // buildx's `no-cache` option: every stage with --no-cache, else --no-cache-filter's.
        no_cache: if parsed.bool("no-cache") {
            Some(Vec::new())
        } else {
            let names: Vec<Vec<u8>> = parsed
                .many("no-cache-filter")
                .iter()
                .flat_map(|v| v.split(','))
                .map(|n| n.as_bytes().to_vec())
                .collect();
            (!names.is_empty()).then_some(names)
        },
        extra_hosts: shards_dockerfile::dockerui::extra_hosts(&add_hosts)
            .map_err(|e| format!("failed to parse additional hosts: {e}"))?,
        shm_size: shards_dockerfile::dockerui::shm_size(parsed.string("shm-size"))
            .map_err(|e| format!("failed to parse shm size: {e}"))?,
        cgroup_parent: parsed.string("cgroup-parent").as_bytes().to_vec(),
        linux_resources: shards_dockerfile::dockerui::linux_resources(&resource_attrs)
            .map_err(|e| format!("failed to parse resource limits: {e}"))?,
        network_mode,
        // As buildx sends it (buildx_attrs): the moby driver resolves images it has.
        image_resolve_mode: if parsed.bool("pull") {
            b"pull".to_vec()
        } else {
            b"local".to_vec()
        },
    };
    let call = call_of(parsed)?;
    let debug = parsed.bool("debug");
    // A subrequest, answered as buildx prints its result (commands/build.go printValue):
    // result.json and a newline for format=json, else the text of it; nothing built.
    let answer = |json: bool, result: String, text: String| {
        let out = if json { format!("{result}\n") } else { text };
        let _ = write!(std::io::stdout(), "{out}");
    };
    let failed = |e: plan::Error| {
        let mut out = String::new();
        for loc in &e.location {
            out.push_str(&excerpt(&name, &text, loc));
        }
        let _ = write!(std::io::stderr(), "{out}");
        print_warnings(&e.warnings, quiet, debug, &name, &text);
        format!("failed to build: failed to solve: {}", show(&e.message))
    };
    match call {
        Some(Call::Outline { json }) => {
            let o = plan::outline(&text, &opts, &bases).map_err(failed)?;
            answer(json, o.json(), o.text());
            return Ok(());
        }
        Some(Call::Targets { json }) => {
            let t = plan::targets(&text, opts.dialect).map_err(failed)?;
            answer(json, t.json(), t.text());
            return Ok(());
        }
        // The build's checks, said as buildx says the lint subrequest's result
        // (commands/build.go printResult): its JSON, or how many warnings, each by line,
        // and the error that ended the planning, which fails the call; nothing built.
        Some(Call::Check { json, ignore_status }) => {
            let lint = plan::lint(&text, &opts, &bases).map_err(failed)?;
            let n = lint.warnings.len();
            let mut out = String::new();
            if json {
                let results = shards_dockerfile::subrequests::LintResults {
                    warnings: &lint.warnings,
                    filename: name.as_bytes(),
                    data: &text,
                    language: match opts.dialect {
                        shards_dockerfile::parser::Dialect::Agentfile => b"Agentfile",
                        shards_dockerfile::parser::Dialect::Dockerfile => b"Dockerfile",
                    },
                    error: lint.error.as_ref().map(|(m, loc)| (m.as_slice(), loc)),
                };
                out.push_str(&results.json());
                out.push('\n');
            } else if n > 0 {
                let found = if n == 1 {
                    "1 warning has been found!".to_string()
                } else {
                    format!("{n} warnings have been found!")
                };
                out.push_str(&format!("Check complete, {found}\n"));
                out.push_str(&lint_text(&lint.warnings, &name, &text));
            }
            if let Some((message, loc)) = &lint.error {
                if !json && n > 0 {
                    out.push('\n');
                }
                let _ = write!(std::io::stdout(), "{out}");
                return Err(format!("{}\n{}", show(message), excerpt(&name, &text, loc)));
            }
            if !json && n == 0 {
                out.push_str("Check complete, no warnings found.\n");
            }
            let _ = write!(std::io::stdout(), "{out}");
            if n > 0 && !ignore_status {
                status.set(1);
            }
            return Ok(());
        }
        Some(Call::Describe { json }) => {
            answer(
                json,
                shards_dockerfile::subrequests::DESCRIBE.to_string(),
                shards_dockerfile::subrequests::describe_text(),
            );
            return Ok(());
        }
        _ => {}
    }
    // Several platforms: each built as one of several, then their one image (D77).
    if target_platforms.len() > 1 && !multi::active() {
        return multi::run(
            parsed,
            status,
            &outputs,
            &target_platforms,
            &progress,
            &provenance_asked,
            &descriptor_annotations,
            pushes,
        );
    }
    let mut plan = match plan::plan(&text, &opts, &bases) {
        Ok(p) => p,
        Err(e) => {
            let mut out = String::new();
            for loc in &e.location {
                out.push_str(&excerpt(&name, &text, loc));
            }
            let _ = write!(std::io::stderr(), "{out}");
            print_warnings(&e.warnings, quiet, debug, &name, &text);
            return Err(format!("failed to build: failed to solve: {}", show(&e.message)));
        }
    };
    {
        // Said once of a build of several platforms, by its first platform's.
        if let Some(found) = &context_ignore
            && multi::with(|sub| sub.first).unwrap_or(true)
        {
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
    // An SBOM asked for (D81): its scanner resolved for the builder's own platform, as
    // BuildKit resolves it with no platform of its own, and run over the build's result
    // and its extra targets after the build's steps.
    let scan = match &sbom_asked {
        Some(asked) => {
            let host = host_platform();
            let resolved = bases
                .resolve(
                    asked.generator.as_bytes(),
                    &host,
                    format!("[internal] load metadata for {}", asked.generator).as_bytes(),
                )
                .map_err(|e| format!("failed to build: failed to solve: {}", show(&e)))?;
            let config =
                shards_dockerfile::image::Image::from_json(&resolved.config).map_err(|e| show(&e))?;
            let id = show(&shards_dockerfile::platform::format_all(&plan.platform));
            let state = plan.state.clone();
            let extras = plan.sbom_extras.clone();
            let pin = resolved.digest.clone().unwrap_or_default();
            let out = sbom::plan(
                &mut plan.graph,
                &resolved.reference,
                &pin,
                &config,
                &host,
                &id,
                &state,
                &extras,
                &asked.params,
            )?;
            let identifier = if resolved.reference.contains(&b'@') || pin.is_empty() {
                [b"docker-image://".as_slice(), &resolved.reference].concat()
            } else {
                [b"docker-image://".as_slice(), &resolved.reference, b"@", &pin].concat()
            };
            Some((out, identifier))
        }
        None => None,
    };
    let (def, scan_at) = match &scan {
        Some((s, _)) => {
            let (def, found) = plan.graph.marshal_with(&plan.state, &[s], &plan.platform);
            (def, found.first().copied().flatten())
        }
        None => (plan.definition(), None),
    };
    // The base a source op names, as it was resolved for the op's platform.
    let op_base = |op: &shards_dockerfile::llb::Op, reference: &str| {
        base_key(reference, op.platform.as_ref().unwrap_or(&opts.target_platform))
    };
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
            .filter(|(_, op)| matches!(op.kind, OpKind::Exec { .. }) && op.platform.as_ref().is_none_or(ours))
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
                        .get(&op_base(op, &reference))
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
    // Every HTTP source is fetched from the start, as BuildKit's solver fetches them; a
    // step waits only for the one it reads.
    let sources: Vec<(usize, String, Option<String>)> = def
        .ops
        .iter()
        .enumerate()
        .filter_map(|(i, op)| match &op.kind {
            OpKind::Source { identifier, attrs } if is_http(identifier) => Some((
                i,
                show(identifier),
                attrs.get(b"http.checksum".as_slice()).map(|c| show(c)),
            )),
            _ => None,
        })
        .collect();
    let dir = if sources.is_empty() {
        PathBuf::new()
    } else {
        exec.stage()?
    };
    let mut downloads = http::Downloads::start(sources, &dir, limits)?;
    let log_limits = LogLimits::from_env();
    let mut builder: Option<builder::Builder> = None;
    // Its steps' seccomp filter, compiled once for the kernel the builder boots.
    let mut step_filter: Vec<u8> = Vec::new();
    let mut results: Vec<Vec<exec::Ref>> = Vec::with_capacity(def.ops.len());
    // The Git and HTTP sources the build read, with what each resolved to, for its
    // provenance.
    let (mut git_materials, mut http_materials): (Vec<provenance::Material>, Vec<provenance::Material>) =
        (Vec::new(), Vec::new());
    // Each operation's cache key (D50), where it has one: none where an input has none.
    let mut keys: Vec<Option<String>> = Vec::with_capacity(def.ops.len());
    let no_cache = parsed.bool("no-cache");
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
        // `why` on the step, and in the build's error after `stage`, what BuildKit was
        // doing when it failed.
        let fail_in = |v: &Vertex, stage: &str, why: &str| {
            progress.borrow().error(v, why);
            print_warnings(&plan.warnings, quiet, debug, &name, &text);
            format!("failed to build: failed to solve: {stage}{why}")
        };
        let fail = |v: &Vertex, why: &str| fail_in(v, "", why);
        // A fetch that failed fails the build on the step of its source.
        let fetch_failed = |op: usize, failure: http::Failure| {
            let named = def
                .metadata
                .get(op)
                .and_then(|m| m.description.get(b"llb.customname".as_slice()))
                .map(|n| show(n))
                .unwrap_or_default();
            let v = progress.borrow_mut().start(&named);
            match failure {
                http::Failure::CacheKey(e) => fail_in(&v, "failed to load cache key: ", &e),
                http::Failure::Snapshot(e) => fail(&v, &e),
            }
        };
        if let Some((op, failure)) = downloads.failed() {
            return Err(fetch_failed(op, failure));
        }
        // A step the build cache holds is not run again: its outputs are the layers it
        // made, as BuildKit's solver takes a vertex its cache key finds.
        let step = matches!(
            op.kind,
            OpKind::Exec { .. } | OpKind::File { .. } | OpKind::Merge | OpKind::Skills { .. }
        );
        let key = if step {
            op.inputs
                .iter()
                .map(|inp| keys.get(inp.op).and_then(|k| k.as_deref()))
                .collect::<Option<Vec<&str>>>()
                .map(|ks| cache::op_key(op, &ks))
        } else {
            None
        };
        // A step marked IgnoreCache (--no-cache-filter's stages) is run, and its result
        // kept, as BuildKit's solver does: the cache is not asked for it.
        if !no_cache
            && !meta.ignore_cache
            && let Some(k) = key.as_deref()
            && let Some(body) = match store.cache_get(k).map_err(|e| e.to_string())? {
                // Not here: from a --cache-from cache, its layers fetched now.
                None if imported.take(k, &store)? => store.cache_get(k).map_err(|e| e.to_string())?,
                held => held,
            }
        {
            let v = progress.borrow_mut().start(&name);
            let outs: Vec<exec::Ref> = cache::decode(&body)
                .and_then(|outputs| {
                    outputs
                        .into_iter()
                        .map(|layers| exec.image(layers, None))
                        .collect()
                })
                .map_err(|e| fail(&v, &e))?;
            progress.borrow().cached(&v);
            guard(meta, &inputs, &outs, &plan.domains, &store).map_err(|e| fail(&v, &e))?;
            store.cache_used(k).map_err(|e| e.to_string())?;
            results.push(outs);
            keys.push(key);
            continue;
        }
        let outs = match &op.kind {
            OpKind::Source { identifier, .. }
                if identifier.starts_with(b"docker-image://") || identifier.starts_with(b"oci-layout://") =>
            {
                let v = progress.borrow_mut().start(&name);
                let reference = show(
                    identifier
                        .strip_prefix(b"docker-image://")
                        .or_else(|| identifier.strip_prefix(b"oci-layout://"))
                        .unwrap_or(identifier),
                );
                progress.borrow().line(&v, &format!("resolve {reference} done"));
                let base = bases
                    .resolved
                    .borrow()
                    .get(&op_base(op, &reference))
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
                let local = identifier.strip_prefix(b"local://").unwrap_or_default();
                let dir = if local == b"context" {
                    context.clone()
                } else {
                    named
                        .locals
                        .get(local)
                        .cloned()
                        .ok_or_else(|| fail(&v, &format!("no local directory {}", show(local))))?
                };
                let r = exec.context(&dir, &filters).map_err(|e| fail(&v, &e))?;
                let (files, bytes) = context_size(&r.fs);
                progress
                    .borrow()
                    .line(&v, &format!("read {files} files, {} done", human_size(bytes)));
                progress.borrow().done(&v);
                vec![r]
            }
            OpKind::Source { identifier, attrs } if is_http(identifier) => {
                let v = progress.borrow_mut().start(&name);
                let download = match downloads.take(i) {
                    Ok(d) => d,
                    Err((op, failure)) if op == i => {
                        return Err(match failure {
                            http::Failure::CacheKey(e) => fail_in(&v, "failed to load cache key: ", &e),
                            http::Failure::Snapshot(e) => fail(&v, &e),
                        });
                    }
                    Err((op, failure)) => {
                        progress.borrow().canceled(&v);
                        return Err(fetch_failed(op, failure));
                    }
                };
                let file = attrs
                    .get(b"http.filename".as_slice())
                    .ok_or_else(|| fail(&v, "an HTTP source without a file name"))?;
                let pin = download.digest.to_string();
                let (algorithm, hex) = pin.split_once(':').unwrap_or(("sha256", pin.as_str()));
                http_materials.push(provenance::Material {
                    uri: redact_credentials(&show(identifier)),
                    algorithm: algorithm.to_string(),
                    hex: hex.to_string(),
                });
                let r = exec.downloaded(download, file).map_err(|e| fail(&v, &e))?;
                progress.borrow().done(&v);
                vec![r]
            }
            OpKind::Source { identifier, .. } if identifier.starts_with(b"osi-artifact://") => {
                let v = progress.borrow_mut().start(&name);
                let reference = show(identifier.strip_prefix(b"osi-artifact://").unwrap_or(identifier));
                let layers = bases
                    .artifacts
                    .borrow()
                    .get(&reference)
                    .cloned()
                    .ok_or_else(|| fail(&v, &format!("{reference}: not resolved")))?;
                let r = exec.image(layers, None).map_err(|e| fail(&v, &e))?;
                progress.borrow().done(&v);
                vec![r]
            }
            OpKind::Skills { name: came_as } => {
                let v = progress.borrow_mut().start(&name);
                let input = inputs
                    .first()
                    .cloned()
                    .ok_or_else(|| fail(&v, "a skills step without what it checks"))?;
                let actions =
                    skills::layout(&input, &show(came_as), &mut exec.sources).map_err(|e| fail(&v, &e))?;
                let outs = exec.file(&[input], &actions, &name).map_err(|e| fail(&v, &e))?;
                progress.borrow().done(&v);
                outs
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
            OpKind::Source { identifier, attrs } if identifier.starts_with(b"git://") => {
                let v = progress.borrow_mut().start(&name);
                let src = git::source(identifier, attrs).map_err(|e| fail(&v, &e))?;
                // A repository's pack, and each object in it, within what a build may unpack.
                let bound = usize::try_from(limits.bytes).unwrap_or(usize::MAX);
                let fetch_limits = shards_git::remote::Limits {
                    pack: bound,
                    object: bound,
                };
                let say = |line: &str| progress.borrow().line(&v, line);
                let cancel = shards_registry::http::Cancel::new();
                let auth = git::auth(&src, &secrets);
                let r = match git::snapshot(
                    &mut exec,
                    &src,
                    fetch_limits,
                    &cancel,
                    auth.as_ref(),
                    &agents,
                    &say,
                ) {
                    Ok((r, commit)) => {
                        // GitIdentifier.Capture: the remote, its ref after a `#`, the commit.
                        let mut uri = redact_credentials(&src.url);
                        if !src.reference.is_empty() {
                            uri.push('#');
                            uri.push_str(&src.reference);
                        }
                        git_materials.push(provenance::Material {
                            uri,
                            algorithm: if commit.len() == 64 { "sha256" } else { "sha1" }.into(),
                            hex: commit,
                        });
                        r
                    }
                    Err(git::Failure::CacheKey(e)) => {
                        return Err(fail_in(&v, "failed to load cache key: ", &e));
                    }
                    Err(git::Failure::Snapshot(e)) => return Err(fail(&v, &e)),
                };
                progress.borrow().done(&v);
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
                // A step for a platform this host's microVMs do not run is refused here, at
                // its turn and before a builder boots: shards emulates no other CPU, where
                // BuildKit runs it under an emulator if one is installed, or fails with
                // the kernel's "exec format error" once it has started.
                if let Some(p) = op.platform.as_ref().filter(|p| !ours(p)) {
                    let why = format!(
                        "shards runs steps for {} only, the platform its microVMs run, not {}: build what runs on the build platform (FROM --platform=$BUILDPLATFORM) and copy it into the {} stage",
                        show(&platform::format(&host_platform())),
                        show(&platform::format(p)),
                        show(&platform::format(p)),
                    );
                    return Err(fail(&v, &why));
                }
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
                                    crate::guest::default(&home, &|_| {}, None, &|k| std::env::var(k).ok())
                                        .map_err(|e| fail(&v, &e))?
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
                    let kernel = crate::guest::version_of(&guest.0).map_err(|e| fail(&v, &e))?;
                    step_filter = crate::setup::step_seccomp(kernel).map_err(|e| fail(&v, &e))?;
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
                    secrets: &secrets,
                    // Granted on `--allow` alone: what BuildKit's daemon must be set up to
                    // grant as well, a builder that is a VM of the build's own grants with
                    // no host to guard (docs/design/architecture.md D33).
                    insecure: allowed.grants(buildflags::SECURITY_INSECURE),
                    network_host: allowed.grants(buildflags::NETWORK_HOST),
                    seccomp: &step_filter,
                    agents: &agents,
                    resources: meta.linux_resources.as_ref(),
                };
                let mut log = StepLog::new(log_limits);
                let r = exec.run(b, &inputs, &op, &name, &mut |which, bytes| {
                    log.write(&progress.borrow(), &v, which, bytes)
                });
                log.end(&progress.borrow(), &v);
                let r = r.map_err(|e| fail(&v, &e))?;
                progress.borrow().done(&v);
                r
            }
        };
        // A step that may write in no domain but its own (D55).
        guard(meta, &inputs, &outs, &plan.domains, &store)
            .map_err(|e| format!("failed to build: failed to solve: {name}: {e}"))?;
        // Its key: a source's from what it holds; a step's, recorded with what it made where
        // its layers make each of its outputs.
        let key = match &op.kind {
            OpKind::Source { identifier, .. } if identifier.starts_with(b"docker-image://") => {
                let digests: Vec<u8> = outs
                    .first()
                    .map(|r| {
                        r.layers
                            .iter()
                            .flat_map(|l| [l.digest.as_slice(), b"\n"].concat())
                            .collect()
                    })
                    .unwrap_or_default();
                Some(cache::source_key(identifier, &String::from_utf8_lossy(&digests)))
            }
            // What it is and what it holds: its attributes include the session's own
            // (`local.unique`), which differ from build to build where its files do not.
            OpKind::Source { identifier, .. } => match outs.first() {
                Some(r) => {
                    let content = cache::content_key(&r.fs, &mut exec.sources)?;
                    Some(cache::source_key(identifier, &content))
                }
                None => None,
            },
            _ => {
                let layered = outs.iter().all(|r| {
                    matches!(r.stack, shards_build::stack::Stack::Known(_)) && r.stack.follows(r.fs.tree())
                });
                if let Some(k) = key.as_deref()
                    && layered
                {
                    let layers: Vec<Vec<Layer>> = outs.iter().map(|r| r.layers.clone()).collect();
                    let blobs = layers
                        .iter()
                        .flatten()
                        .map(|l| Digest::parse(&show(&l.digest)).map_err(|e| e.to_string()))
                        .collect::<Result<Vec<_>, String>>()?;
                    // What the step made itself: its root output's last layer.
                    let own = outs.first().and_then(|r| r.layers.last()).map_or(0, |l| l.size);
                    store
                        .cache_put(k, &blobs, own, &cache::encode(&layers)?)
                        .map_err(|e| e.to_string())?;
                }
                key
            }
        };
        results.push(outs);
        keys.push(key);
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
                            .get(&op_base(op, &reference))
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
    // The SBOMs the scan wrote (D81), each a statement, the core target's first.
    let sboms: Vec<sbom::Scanned> = match scan_at {
        Some(at) => {
            let r = usize::try_from(at.index)
                .ok()
                .and_then(|i| results.get(at.op).and_then(|outs| outs.get(i)))
                .ok_or("the SBOM scan has no output")?;
            sbom::read(r.fs.tree(), &mut exec.sources, limits.bytes)
                .map_err(|e| format!("failed to build: failed to solve: {e}"))?
        }
        None => Vec::new(),
    };
    // mode=max: each step output's layers, as the build's records name them (D80).
    let wants_max = matches!(provenance_asked, Provenance::Explicit { max: true, .. });
    let chains: Vec<(usize, usize, Vec<Vec<u8>>)> = if wants_max {
        results
            .iter()
            .enumerate()
            .flat_map(|(op, outs)| {
                outs.iter()
                    .enumerate()
                    .map(move |(i, r)| (op, i, r.layers.iter().map(|l| l.digest.clone()).collect()))
            })
            .collect()
    } else {
        Vec::new()
    };
    // Every layer is in the store now. The root filesystem is written from the target's
    // snapshot, put in its layers' form, its files read where the build has them; the
    // other snapshots go first. Where that cannot be done exactly, the snapshots, their
    // sources and stages all go before the export stacks the layers again, so the two
    // never hold memory at once.
    drop(results);
    // What the build's provenance says of it, where an output carries one (D71, D72):
    // every source has resolved now.
    let invocation = build_ref()?;
    let mut capture = {
        let mut materials = provenance::capture_images(&def, scan.as_ref().map(|(_, id)| id.as_slice()));
        for mut list in [
            std::mem::take(&mut git_materials),
            std::mem::take(&mut http_materials),
        ] {
            list.sort_by(|a, b| a.uri.cmp(&b.uri));
            list.dedup_by(|a, b| a.uri == b.uri);
            materials.extend(list);
        }
        let (secrets, ssh, network) = provenance::capture_mounts(&def);
        provenance::Capture {
            args: request_attrs.clone(),
            materials,
            locals: request_locals.clone(),
            secrets,
            ssh,
            network,
            max: None,
            sboms,
        }
    };
    // Whether the image's attestation holds its provenance: by default where an image is
    // stored or pushed (unless BUILDX_NO_DEFAULT_ATTESTATIONS), always where asked for.
    let provenance_on = match &provenance_asked {
        Provenance::Default => attests(&|k| std::env::var(k).ok())?,
        Provenance::Off => false,
        Provenance::Explicit { .. } => true,
    };
    let (builder_id, reproducible) = match &provenance_asked {
        Provenance::Explicit {
            builder_id,
            reproducible,
            ..
        } => (builder_id.clone(), *reproducible),
        _ => (String::new(), false),
    };
    let facts = |platform: &str| provenance::Run {
        invocation_id: invocation.clone(),
        started,
        finished: unix_now(),
        builder_platform: platform.to_string(),
        builder_id: builder_id.clone(),
        reproducible,
    };
    let host_platform_shown = show(&shards_dockerfile::platform::format(&host_platform()));
    // An explicit provenance, not inline-only, goes in every output.
    let everywhere = matches!(
        provenance_asked,
        Provenance::Explicit {
            inline_only: false,
            ..
        }
    );
    let everywhere_facts = facts(&host_platform_shown);
    // mode=max (D80): the definition as steps, its source map, and the layers each step's
    // output is, so far the bases' (a local output is written before the image's).
    if wants_max {
        let steps = provenance::steps(&def);
        let info_def = provenance::dockerfile_definition(&filename, "dockerfile", &invocation);
        let info_steps = provenance::steps(&info_def);
        capture.max = Some(provenance::Max {
            build_config: provenance::build_config(&steps),
            source: provenance::source(&def, &steps, (&filename, &text), &info_steps),
            layers: step_layers(&chains, &steps, &base_layers(&bases)),
        });
    }
    // What every output's attestation is made of: an explicit provenance not inline-only,
    // or an SBOM, beside which a default provenance goes too (D81).
    let everywhere_provenance =
        (everywhere || !capture.sboms.is_empty()).then_some((&capture, &everywhere_facts));
    // Each domain's isolation, before anything leaves the build (§9.2).
    if let Some(r) = &target
        && !plan.domains.is_empty()
    {
        let v = progress
            .borrow_mut()
            .start("[internal] checking the domains' isolation");
        domains::check(&r.fs, &plan.domains).map_err(|e| fail_export(&progress, &v, &e))?;
        progress.borrow().done(&v);
    }
    // The filesystem outputs, from the snapshot itself: its times to the nanosecond,
    // which its layers' headers keep to the second.
    for o in outputs.iter().filter(|o| o.kind == "local" || o.kind == "tar") {
        let empty;
        let fs = match &target {
            Some(r) => &*r.fs,
            None => {
                empty = shards_build::vfs::Fs::new(
                    shards_image::erofs::Tree::new(shards_image::erofs::Meta {
                        mode: 0o755,
                        ..Default::default()
                    }),
                    exec::now(),
                );
                &empty
            }
        };
        write_fs_output(
            o,
            fs,
            &mut exec.sources,
            plan.epoch,
            &progress,
            everywhere_provenance,
            everywhere,
        )?;
    }
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

    // The layers as the image holds them: those the build made compressed as its
    // exporter asks (D75), the config naming them uncompressed, by DiffID, as ever.
    let existing: std::collections::BTreeSet<Vec<u8>> = bases
        .resolved
        .borrow()
        .values()
        .flat_map(|b| b.layers.iter().map(|l| l.digest.clone()))
        .chain(
            bases
                .artifacts
                .borrow()
                .values()
                .flatten()
                .map(|l| l.digest.clone()),
        )
        .collect();
    // rewrite-timestamp, at the build's SOURCE_DATE_EPOCH or the outputs' own; without
    // one, BuildKit's warning, which its daemon's log alone shows.
    let rewrite_epoch = if rewrite_timestamp {
        let at = outputs
            .iter()
            .find_map(|o| output::epoch(&o.attrs, plan.epoch).ok().flatten())
            .or(plan.epoch);
        if at.is_none() {
            let _ = writeln!(
                std::io::stderr(),
                "WARNING: rewrite-timestamp is specified, but no source-date-epoch was found"
            );
        }
        at
    } else {
        None
    };
    let base_diff_ids: Vec<Vec<u8>> = base_image
        .as_ref()
        .and_then(|b| b.rootfs.diff_ids.clone())
        .unwrap_or_default();
    let rewrite = rewrite_epoch.map(|epoch| compress::Rewrite {
        epoch,
        base: &base_diff_ids,
    });
    let held = compress::layers(&store, &layers, &existing, compression, rewrite, &limits)?;
    // Each layer the image holds by the digest the build's records name it by.
    let as_held: BTreeMap<Vec<u8>, Layer> = layers
        .iter()
        .zip(&held)
        .map(|(l, h)| (l.digest.clone(), h.clone()))
        .collect();
    // mode=max, now that the image's layers are written: each step's layers again, the
    // image's among them.
    let capture = match &capture.max {
        Some(m) => {
            let mut known = base_layers(&bases);
            known.extend(as_held.iter().map(|(k, v)| (k.clone(), v.clone())));
            let mut c = capture.clone();
            c.max = Some(provenance::Max {
                layers: step_layers(&chains, &provenance::steps(&def), &known),
                ..m.clone()
            });
            c
        }
        None => capture.clone(),
    };
    // What every output's attestation is made of: an explicit provenance not inline-only,
    // or an SBOM, beside which a default provenance goes too (D81).
    let everywhere_provenance =
        (everywhere || !capture.sboms.is_empty()).then_some((&capture, &everywhere_facts));
    let epoch = plan.epoch.map(Time::from_unix);
    // From the layers as written: a rewrite changes their DiffIDs.
    let config = export::config(&plan.image, &held, epoch, base_image.as_ref()).map_err(|e| show(&e))?;
    // The build's records, for --cache-to, and the image's layers, which `min` and an
    // inline cache keep to.
    let used: Vec<String> = keys.iter().flatten().cloned().collect();
    let image_layers: std::collections::BTreeSet<String> = layers.iter().map(|l| show(&l.digest)).collect();
    // One platform's build of several: what the builds of them all export (D87).
    multi::with(|sub| {
        sub.keys.extend(used.iter().cloned());
        sub.image_layers.extend(image_layers.iter().cloned());
    });
    let config = if cache_to.iter().any(|e| e.kind == "inline") {
        remote::inline(&config, &used, &store, &image_layers, &as_held)?
    } else {
        config
    };
    let config_digest = sha256(&config);
    // An Agentfile's image: findable by its manifest's annotations (D57).
    // --annotation's, then the Agentfile's own, which no annotation asked for replaces.
    let mut annotations: BTreeMap<Vec<u8>, Vec<u8>> = manifest_annotations
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    if let Some(digest) = plan
        .image
        .config
        .labels
        .get(shards_dockerfile::agentfile::DIGEST_LABEL)
    {
        annotations.insert(
            shards_dockerfile::agentfile::DIGEST_LABEL.to_vec(),
            digest.clone(),
        );
        let names = |harness: bool| -> Vec<u8> {
            let mut n: Vec<&[u8]> = plan
                .domains
                .iter()
                .filter(|d| d.harness == harness)
                .map(|d| d.name.as_slice())
                .collect();
            n.sort_unstable();
            n.dedup();
            n.join(&b","[..])
        };
        for (key, harness) in [
            (shards_dockerfile::agentfile::AGENTS_ANNOTATION, false),
            (shards_dockerfile::agentfile::HARNESSES_ANNOTATION, true),
        ] {
            let list = names(harness);
            if !list.is_empty() {
                annotations.insert(key.to_vec(), list);
            }
        }
    }
    let raw_layers = layers;
    let layers = held;
    // The manifest in OCI's types, which layouts name; and the stored and pushed image's,
    // in the types its output asks for (`oci-mediatypes`), else OCI's where it is attested
    // or annotated and Docker's otherwise, as BuildKit v0.28.1's image exporter defaults
    // (DefaultOCITypes; measured: a stored build with --provenance=false has Docker's, D83).
    let oci_manifest = export::manifest_annotated(
        &config,
        config_digest.to_string().as_bytes(),
        &layers,
        &annotations,
    );
    let oci_manifest_digest = sha256(&oci_manifest);
    let stored_oci = match outputs
        .iter()
        .filter(|o| matches!(o.kind.as_str(), "image" | "moby"))
        .find_map(|o| o.attrs.get("oci-mediatypes"))
    {
        Some(v) => oci_types_attr(v)?,
        None => provenance_on || !capture.sboms.is_empty() || !annotations.is_empty(),
    };
    let (manifest, manifest_type) = if stored_oci {
        (oci_manifest.clone(), oci::media::OCI_MANIFEST)
    } else {
        (
            export::docker_manifest(&config, config_digest.to_string().as_bytes(), &layers),
            "application/vnd.docker.distribution.manifest.v2+json",
        )
    };
    let manifest_digest = sha256(&manifest);
    // Each file output that compresses or rewrites its layers otherwise than the image
    // (D85): its own layers, written as it asks (each once, the build cache's records
    // kept), its config and its manifest.
    let mut variants: Vec<(usize, Variant)> = Vec::new();
    for (i, o) in outputs.iter().enumerate() {
        if !matches!(o.kind.as_str(), "oci" | "docker") || matches!(o.dest, buildflags::Dest::Store) {
            continue;
        }
        let (c, rw) = output_compression(o)?;
        if (c, rw) == (compression, rewrite_timestamp) {
            continue;
        }
        let at = if rw {
            output::epoch(&o.attrs, plan.epoch)
                .map_err(|e| format!("failed to build: failed to solve: {e}"))?
                .or(plan.epoch)
        } else {
            None
        };
        let rewrite = at.map(|epoch| compress::Rewrite {
            epoch,
            base: &base_diff_ids,
        });
        let held = compress::layers(&store, &raw_layers, &existing, c, rewrite, &limits)?;
        let config = export::config(&plan.image, &held, epoch, base_image.as_ref()).map_err(|e| show(&e))?;
        let config_digest = sha256(&config);
        let manifest =
            export::manifest_annotated(&config, config_digest.to_string().as_bytes(), &held, &annotations);
        let manifest_digest = sha256(&manifest);
        variants.push((
            i,
            Variant {
                layers: held,
                config,
                config_digest,
                manifest,
                manifest_digest,
            },
        ));
    }
    // Named and kept where an output loads it (the image exporter, `--load`'s docker one,
    // or none asked for, docker build's own); an output to a file or directory alone
    // leaves no image, as BuildKit's exporters leave none.
    let kept = outputs.is_empty()
        || outputs.iter().any(|o| {
            matches!(o.dest, buildflags::Dest::Store)
                && matches!(o.kind.as_str(), "image" | "moby" | "docker")
        });
    // One platform's build of several attests its layout always: the build of them
    // decides what its image carries (D77).
    let layout_provenance = if multi::active() {
        Some((&capture, &everywhere_facts))
    } else {
        everywhere_provenance
    };
    if !kept && !pushes {
        drop(exec);
        drop(flat);
        write_image_outputs(
            &outputs,
            &store,
            &output::Made {
                config: &config,
                config_digest: &config_digest,
                manifest: &oci_manifest,
                manifest_digest: &oci_manifest_digest,
                layers: &layers,
                descriptor_annotations: &descriptor_annotations,
                provenance: layout_provenance,
                provenance_in: multi::active() || everywhere || provenance_on,
            },
            &variants,
            parsed.many("tag"),
            plan.epoch,
            &progress,
        )?;
        remote::export(&cache_to, &used, &store, &image_layers, &progress, &env)?;
        // An image in an OCI or docker archive is one; a directory or tar of files none.
        let imaged = outputs.iter().any(|o| o.kind == "oci" || o.kind == "docker");
        let names = canonical_names(parsed.many("tag"))?;
        // An attested image in an OCI archive is named by the index the layout holds.
        let tagged: Vec<Reference> = parsed
            .many("tag")
            .iter()
            .filter_map(|t| Reference::parse(t).ok())
            .collect();
        let index = output::attested_index(
            &output::Made {
                config: &config,
                config_digest: &config_digest,
                manifest: &oci_manifest,
                manifest_digest: &oci_manifest_digest,
                layers: &layers,
                descriptor_annotations: &descriptor_annotations,
                provenance: everywhere_provenance,
                provenance_in: everywhere || provenance_on,
            },
            &tagged,
        )?;
        let (described, size, media) = match &index {
            Some((d, n)) if imaged => (d, *n, oci::media::OCI_INDEX),
            _ => (&oci_manifest_digest, oci_manifest.len(), oci::media::OCI_MANIFEST),
        };
        let info = everywhere.then(|| provenance::buildinfo(&capture, &everywhere_facts));
        multi::with(|sub| {
            sub.provenance = Some(provenance::buildinfo(&capture, &everywhere_facts));
            sub.next = progress.borrow().next;
            sub.answered = bases.answered.take();
            sub.bases = bases.resolved.take();
        });
        write_metadata(
            parsed,
            &metadata(
                &invocation,
                imaged.then_some((described, size, names.as_slice())),
                &descriptor_annotations,
                &info.iter().map(|p| (None, p)).collect::<Vec<_>>(),
                media,
            ),
        )?;
        print_warnings(&plan.warnings, quiet, debug, &name, &text);
        return finish(parsed, &described.to_string());
    }
    let v = progress.borrow_mut().start("exporting to image");
    progress.borrow().line(&v, "exporting layers done");
    store
        .ingest(&config_digest, config.len() as u64, &mut config.as_slice())
        .map_err(|e| e.to_string())?;
    store
        .ingest(&manifest_digest, manifest.len() as u64, &mut manifest.as_slice())
        .map_err(|e| e.to_string())?;
    progress
        .borrow()
        .line(&v, &format!("writing image {manifest_digest} done"));
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
        // Rewritten layers are not the snapshot's times: the root filesystem is theirs.
        _ if rewrite_epoch.is_some() => {
            store.rootfs(&store_layers, &limits).map_err(|e| e.to_string())?;
            crate::phase("rootfs-from-layers");
        }
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
        media_type: manifest_type.into(),
        digest: manifest_digest.to_string(),
        size: i64::try_from(manifest.len()).map_err(|e| e.to_string())?,
        platform: None,
        annotations: Default::default(),
    };
    let mut contents = vec![manifest_digest.clone(), config_digest.clone()];
    contents.extend(store_layers.iter().map(|l| l.blob.clone()));
    // Its provenance (D71), as buildx asks BuildKit for it by default where an image is
    // stored or pushed: the statement, the attestation that holds it, and the index of
    // the image and its attestation, which the image's ID then names.
    let attest = provenance_on;
    let attested = if attest || !capture.sboms.is_empty() {
        let image_config: serde_json::Value = serde_json::from_slice(&config).map_err(|e| e.to_string())?;
        let field = |k: &str| {
            image_config
                .get(k)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (arch, os, variant) = (field("architecture"), field("os"), field("variant"));
        let mut platform = format!("{os}/{arch}");
        if !variant.is_empty() && !(arch == "arm64" && variant == "v8") {
            platform.push_str(&format!("/{variant}"));
        }
        let facts = facts(&platform);
        let subjects: Vec<(String, String)> = parsed
            .many("tag")
            .iter()
            .filter_map(|t| Reference::parse(t).ok())
            .map(|r| (provenance::image_purl(&r, &platform), manifest_digest.to_string()))
            .collect();
        let statements = provenance::image_statements(&capture, &facts, &subjects, attest);
        let digests: Vec<String> = statements
            .iter()
            .map(|(st, _)| sha256(st.as_bytes()).to_string())
            .collect();
        let listed: Vec<(&str, usize, &str)> = statements
            .iter()
            .zip(&digests)
            .map(|((st, kind), d)| (d.as_str(), st.len(), kind.as_str()))
            .collect();
        let (att_config, att_manifest) = provenance::attestation_of(&listed);
        let att_manifest_digest = sha256(att_manifest.as_bytes());
        let index = provenance::index(
            (&manifest_digest.to_string(), manifest.len()),
            (
                &arch,
                &os,
                if arch == "arm64" && variant == "v8" {
                    ""
                } else {
                    &variant
                },
            ),
            (&att_manifest_digest.to_string(), att_manifest.len()),
        );
        let index_digest = sha256(index.as_bytes());
        let mut blobs: Vec<&[u8]> = statements.iter().map(|(st, _)| st.as_bytes()).collect();
        blobs.extend([att_config.as_bytes(), att_manifest.as_bytes(), index.as_bytes()]);
        for blob in blobs {
            let digest = sha256(blob);
            store
                .ingest(&digest, blob.len() as u64, &mut &blob[..])
                .map_err(|e| e.to_string())?;
            contents.push(digest);
        }
        Some((index_digest, index.len(), provenance::buildinfo(&capture, &facts)))
    } else {
        None
    };
    // What the image's names resolve to, and its ID: the index where it is attested.
    let id = attested.as_ref().map_or(&manifest_digest, |(d, _, _)| d).clone();
    let tags: &[String] = if kept { parsed.many("tag") } else { &[] };
    for tag in tags {
        let reference = Reference::parse(tag).map_err(|e| format!("invalid tag {tag:?}: {e}"))?;
        store
            .tag(&reference.to_string(), &desc, &id, &contents)
            .map_err(|e| e.to_string())?;
        progress.borrow().line(&v, &format!("naming to {reference} done"));
    }
    // Unnamed, it is kept all the same, dangling, as dockerd keeps a build it was given no
    // name for (moby daemon/containerd/image_builder.go): `images -a` lists it, and a
    // collection leaves it.
    if kept && tags.is_empty() {
        let dangling = format!("{}{id}", store::DANGLING);
        store
            .tag(&dangling, &desc, &id, &contents)
            .map_err(|e| e.to_string())?;
    }
    // An image output that pushes: each of its names, as BuildKit's exporter pushes
    // them (util/push): its layers, then its manifest, then the index that names it.
    if pushes {
        let pushed = match &attested {
            Some((index_digest, size, _)) => Descriptor {
                media_type: oci::media::OCI_INDEX.into(),
                digest: index_digest.to_string(),
                size: i64::try_from(*size).map_err(|e| e.to_string())?,
                platform: None,
                annotations: Default::default(),
            },
            None => desc.clone(),
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
            .map_err(|e| fail_export(&progress, &v, &e.to_string()))?;
            progress
                .borrow()
                .line(&v, &format!("pushing manifest for {reference}@{id} done"));
        }
    }
    progress.borrow().done(&v);
    write_image_outputs(
        &outputs,
        &store,
        &output::Made {
            config: &config,
            config_digest: &config_digest,
            manifest: &oci_manifest,
            manifest_digest: &oci_manifest_digest,
            layers: &layers,
            descriptor_annotations: &descriptor_annotations,
            provenance: everywhere_provenance,
            provenance_in: everywhere || provenance_on,
        },
        &variants,
        parsed.many("tag"),
        plan.epoch,
        &progress,
    )?;
    remote::export(&cache_to, &used, &store, &image_layers, &progress, &env)?;
    let names = canonical_names(parsed.many("tag"))?;
    let (described, size, media) = match &attested {
        Some((d, size, _)) => (d, *size, oci::media::OCI_INDEX),
        None => (&manifest_digest, manifest.len(), oci::media::OCI_MANIFEST),
    };
    write_metadata(
        parsed,
        &metadata(
            &invocation,
            Some((described, size, names.as_slice())),
            &descriptor_annotations,
            &attested.iter().map(|(_, _, p)| (None, p)).collect::<Vec<_>>(),
            media,
        ),
    )?;
    print_warnings(&plan.warnings, quiet, debug, &name, &text);
    finish(parsed, &id.to_string())
}

/// The build's ID where it is asked for: in `--iidfile`, and with `-q` on stdout.
/// BuildKit's `identity.NewID`: 17 random bytes, the first's high bit set, in base 36,
/// its first digit dropped: 25 characters.
/// The base images' layers, by the digest the build's records name each by.
fn base_layers(bases: &Bases) -> BTreeMap<Vec<u8>, Layer> {
    let mut known = BTreeMap::new();
    for b in bases.resolved.borrow().values() {
        for l in &b.layers {
            known.insert(l.digest.clone(), l.clone());
        }
    }
    known
}

/// `buildkit_metadata.layers` (mode=max, D80): each step output's layers, said where each
/// is a blob the build has (`known`), as BuildKit's cache exporter finds them
/// (CacheExportModeRemoteOnly); a step whose layers were never written, as a stage only
/// copied from, has none. Keys in order, as Go writes a map.
fn step_layers(
    chains: &[(usize, usize, Vec<Vec<u8>>)],
    steps: &provenance::Steps,
    known: &BTreeMap<Vec<u8>, Layer>,
) -> Vec<(String, provenance::Json)> {
    let mut out: Vec<(String, provenance::Json)> = Vec::new();
    for (op, index, chain) in chains {
        let Some(n) = steps.step_of.get(*op).copied().flatten() else {
            continue;
        };
        if chain.is_empty() {
            continue;
        }
        let descriptors: Option<Vec<provenance::Json>> = chain
            .iter()
            .map(|d| {
                known
                    .get(d)
                    .map(|l| provenance::layer_descriptor(&show(&l.media_type), &show(&l.digest), l.size))
            })
            .collect();
        if let Some(d) = descriptors {
            out.push((
                format!("step{n}:{index}"),
                provenance::Json::Arr(vec![provenance::Json::Arr(d)]),
            ));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

fn build_ref() -> Result<String, String> {
    let mut n = [0u8; 17];
    shards_vmm::platform::fill_random(&mut n).map_err(|e| e.to_string())?;
    if let Some(b) = n.first_mut() {
        *b |= 0x80;
    }
    let mut digits = Vec::new();
    let mut rest: Vec<u8> = n.to_vec();
    while rest.iter().any(|&b| b != 0) {
        let mut carry = 0u32;
        for b in &mut rest {
            let cur = (carry << 8) | u32::from(*b);
            *b = u8::try_from(cur / 36).unwrap_or(0);
            carry = cur % 36;
        }
        digits.push(
            b"0123456789abcdefghijklmnopqrstuvwxyz"
                .get(carry as usize)
                .copied()
                .unwrap_or(b'0'),
        );
    }
    digits.reverse();
    Ok(String::from_utf8_lossy(digits.get(1..26).unwrap_or_default()).into_owned())
}

/// What `--metadata-file` holds, as buildx writes it (commands/build.go
/// `decodeExporterResponse`, `writeMetadataFile`): the build's reference, and for an image
/// what Docker's image exporter answers with its containerd store, which names an image by
/// its manifest's digest, as shards' store does: the manifest's descriptor and digest, and
/// the image's names, and no config digest, so that the ID buildx takes of it
/// (`getImageID`) is the image's. Keys in order, as Go's `MarshalIndent` writes a map.
fn metadata(
    build_ref: &str,
    image: Option<(&Digest, usize, &[String])>,
    annotations: &BTreeMap<String, String>,
    provenance: &[(Option<&str>, &provenance::Json)],
    media_type: &str,
) -> String {
    let q = |s: &str| serde_json::Value::from(s).to_string();
    let mut out = String::from("{");
    // buildx's provenance of the build (v0.2), first of the keys in order; one for each
    // platform of several, `buildx.build.provenance/<platform>`.
    for (platform, p) in provenance {
        let key = match platform {
            Some(pl) => format!("buildx.build.provenance/{pl}"),
            None => "buildx.build.provenance".to_string(),
        };
        out.push_str(&format!("\n  {}: {},", q(&key), p.indented(1)));
    }
    out.push_str(&format!(
        "\n  \"buildx.build.ref\": {}",
        q(&format!("shards/shards/{build_ref}"))
    ));
    if let Some((manifest, size, names)) = image {
        out.push_str(&format!(
            ",\n  \"containerimage.descriptor\": {{\n    \"mediaType\": {},\n    \"digest\": {},\n    \"size\": {size}{}\n  }},\n  \"containerimage.digest\": {}",
            q(media_type),
            q(&manifest.to_string()),
            // ocispec.Descriptor's annotations, after its size, keys in order.
            if annotations.is_empty() {
                String::new()
            } else {
                let pairs: Vec<String> = annotations
                    .iter()
                    .map(|(k, v)| format!("\n      {}: {}", q(k), q(v)))
                    .collect();
                format!(",\n    \"annotations\": {{{}\n    }}", pairs.join(","))
            },
            q(&manifest.to_string()),
        ));
        if !names.is_empty() {
            out.push_str(&format!(",\n  \"image.name\": {}", q(&names.join(","))));
        }
    }
    out.push_str("\n}");
    out
}

/// Annotations by key.
type Annotations = BTreeMap<String, String>;

/// The annotations asked for, by `--annotation` and by an image output's `annotation…`
/// attributes (exptypes `ParseAnnotationKey`), as BuildKit's image exporter applies them to
/// an image of one platform (exporter/containerimage/writer.go): the manifest's and its
/// descriptor's; an index's refused, as there. One for a platform applies where it is the
/// image's, which BuildKit leaves out of a single platform's export (`Platform(nil)`), and
/// one for another is refused in the words BuildKit refuses a platform the build lacks.
fn annotations_of(
    parsed: &Parsed,
    outputs: &[buildflags::Output],
    target: &Platform,
) -> Result<(Annotations, Annotations), String> {
    let mut all = buildflags::parse_annotations(parsed.many("annotation"))?;
    let key_re = regex::Regex::new(r"^annotation(?:-([a-z-]+))?(?:\[([A-Za-z0-9_/-]+)\])?\.(\S+)$")
        .map_err(|e| e.to_string())?;
    for o in outputs
        .iter()
        .filter(|o| matches!(o.kind.as_str(), "image" | "moby" | "oci" | "docker"))
    {
        for (k, v) in &o.attrs {
            let Some(g) = key_re.captures(k) else { continue };
            let kind = g.get(1).map_or("", |m| m.as_str());
            if !matches!(
                kind,
                "" | "index" | "index-descriptor" | "manifest" | "manifest-descriptor"
            ) {
                return Err(format!("unrecognized annotation type {kind}"));
            }
            all.push(buildflags::Annotation {
                kind: kind.to_string(),
                platform: g.get(2).map(|m| m.as_str().to_string()),
                key: g.get(3).map_or("", |m| m.as_str()).to_string(),
                value: v.clone(),
            });
        }
    }
    let host = host_platform();
    let (mut manifest, mut descriptor) = (BTreeMap::new(), BTreeMap::new());
    for a in all {
        if let Some(p) = &a.platform {
            // The platform's ID, as BuildKit finds a ref by it (FindRef): normalized, in full.
            let wanted = platform::normalize(&platform::parse(p.as_bytes(), &host).map_err(|e| show(&e))?);
            if platform::format_all(&wanted) != platform::format_all(target) {
                return Err(format!("invalid annotation: no platform {p} found in source"));
            }
        }
        match a.kind.as_str() {
            "" | "manifest" => {
                manifest.insert(a.key, a.value);
            }
            "manifest-descriptor" => {
                descriptor.insert(a.key, a.value);
            }
            _ => return Err("index annotations not supported for single platform export".into()),
        }
    }
    Ok((manifest, descriptor))
}

/// The image's names as BuildKit's exporter has them: each tag in full.
fn canonical_names(tags: &[String]) -> Result<Vec<String>, String> {
    tags.iter()
        .map(|t| {
            Reference::parse(t)
                .map(|r| r.to_string())
                .map_err(|e| format!("invalid tag {t:?}: {e}"))
        })
        .collect()
}

/// Writes `--metadata-file`, whole or not at all, as buildx's atomic writer does.
fn write_metadata(parsed: &Parsed, text: &str) -> Result<(), String> {
    let path = parsed.string("metadata-file");
    if path.is_empty() {
        return Ok(());
    }
    let path = Path::new(path);
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn finish(parsed: &Parsed, id: &str) -> Result<(), String> {
    // One platform's build of several says nothing of its own: the build of them does.
    if multi::active() {
        return Ok(());
    }
    let iidfile = parsed.string("iidfile");
    if !iidfile.is_empty() {
        std::fs::write(iidfile, id).map_err(|e| format!("{iidfile}: {e}"))?;
    }
    if parsed.bool("quiet") {
        let _ = writeln!(std::io::stdout(), "{id}");
    }
    crate::phase("end");
    Ok(())
}

/// The compression the image's layers take (D75): what the outputs that hold an image
/// ask, BuildKit's default where none asks; outputs that ask differently are refused,
/// named, until each is written its own way.
/// `oci-mediatypes`, read as ImageCommitOpts.Load reads it.
fn oci_types_attr(v: &str) -> Result<bool, String> {
    shards_cmdline::go::parse_bool(v).map_err(|e| {
        format!("failed to build: failed to solve: non-bool value specified for oci-mediatypes: {e}")
    })
}

/// An image output's compression and whether it rewrites its layers' times, as BuildKit
/// reads them (`ParseAttributes`; ImageCommitOpts.Load's parseBool, in its words).
fn output_compression(o: &buildflags::Output) -> Result<(compress::Compression, bool), String> {
    let solve = |e: String| format!("failed to build: failed to solve: {e}");
    let rewrite = match o.attrs.get("rewrite-timestamp") {
        None => false,
        Some(v) => shards_cmdline::go::parse_bool(v)
            .map_err(|e| solve(format!("non-bool value specified for rewrite-timestamp: {e}")))?,
    };
    let c = compress::Compression::of(&o.attrs).map_err(solve)?;
    // `Validate`: eStargz is OCI's alone.
    if c.kind == compress::Kind::Estargz
        && let Some(v) = o.attrs.get("oci-mediatypes")
        && !oci_types_attr(v)?
    {
        return Err(solve(
            "exporter option \"compression=estargz\" conflicts with \"oci-mediatypes=false\"".into(),
        ));
    }
    Ok((c, rewrite))
}

/// The image's compression and rewrite: the stored image's output's, else the first image
/// output's; every image output's checked. Each output writes its layers as it asks
/// (D85): one asking otherwise is given its own.
fn compression_of(outputs: &[buildflags::Output]) -> Result<(compress::Compression, bool), String> {
    let mut asked = Vec::new();
    for o in outputs
        .iter()
        .filter(|o| matches!(o.kind.as_str(), "image" | "moby" | "docker" | "oci"))
    {
        asked.push((
            matches!(o.kind.as_str(), "image" | "moby"),
            output_compression(o)?,
        ));
    }
    Ok(asked
        .iter()
        .find(|(stored, _)| *stored)
        .or(asked.first())
        .map(|(_, c)| *c)
        .unwrap_or_default())
}

/// What BuildKit refuses of the outputs when its solve begins, before any step runs:
/// an exporter it has none of, a `tar` attribute no bool, an epoch no number, the
/// `docker` exporter into a directory (the moby exporter writes none), and two OCI
/// layouts into directories (their store's key is one, `export`: buildkit
/// client/solve.go).
fn check_outputs(outputs: &[buildflags::Output]) -> Result<(), String> {
    let mut layouts = 0;
    for o in outputs {
        let solve = |why: String| format!("failed to build: failed to solve: {why}");
        match o.kind.as_str() {
            "image" | "moby" | "local" | "tar" | "oci" | "docker" => {}
            other => {
                return Err(solve(format!(
                    "exporter {} could not be found",
                    shards_cmdline::go::quote(other)
                )));
            }
        }
        if matches!(o.kind.as_str(), "oci" | "docker")
            && let Some(t) = o.attrs.get("tar")
            && shards_cmdline::go::parse_bool(t).is_err()
        {
            return Err(solve(format!(
                "non-bool value specified for tar: strconv.ParseBool: parsing {}: invalid syntax",
                shards_cmdline::go::quote(t)
            )));
        }
        output::epoch(&o.attrs, None).map_err(solve)?;
        if let buildflags::Dest::Dir(_) = o.dest {
            match o.kind.as_str() {
                "docker" => {
                    return Err("failed to build: output directory is not supported by moby exporter".into());
                }
                "oci" => {
                    layouts += 1;
                    if layouts > 1 {
                        return Err("failed to build: oci store key \"export\" already exists".into());
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// A `local` or `tar` output of the snapshot `fs`.
fn write_fs_output(
    o: &buildflags::Output,
    fs: &shards_build::vfs::Fs,
    sources: &mut shards_build::data::Sources,
    build_epoch: Option<i64>,
    progress: &RefCell<Progress>,
    provenance: Option<(&provenance::Capture, &provenance::Run)>,
    explicit: bool,
) -> Result<(), String> {
    let epoch =
        output::epoch(&o.attrs, build_epoch).map_err(|e| format!("failed to build: failed to solve: {e}"))?;
    if o.kind == "local" {
        let v = progress.borrow_mut().start("exporting to client directory");
        let buildflags::Dest::Dir(dest) = &o.dest else {
            return Err(fail_export(progress, &v, "a local output without a directory"));
        };
        let mirror = o
            .attrs
            .get("mode")
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("delete"));
        let bytes =
            output::local(fs, sources, epoch, dest, mirror).map_err(|e| fail_export(progress, &v, &e))?;
        if let Some((capture, run)) = provenance {
            output::local_attestations(fs, dest, capture, run, explicit)
                .map_err(|e| fail_export(progress, &v, &e))?;
        }
        progress
            .borrow()
            .line(&v, &format!("copying files {} done", units_bytes(bytes)));
        progress.borrow().done(&v);
        return Ok(());
    }
    let v = progress.borrow_mut().start("exporting to client tarball");
    let written = match &o.dest {
        buildflags::Dest::Stdout => {
            let out = std::io::BufWriter::new(std::io::stdout().lock());
            output::tar(fs, sources, epoch, out).and_then(|mut w| w.flush().map_err(|e| e.to_string()))
        }
        buildflags::Dest::File(path) => create_dest_file(path)
            .and_then(|f| output::tar(fs, sources, epoch, std::io::BufWriter::new(f)))
            .and_then(|mut w| w.flush().map_err(|e| e.to_string())),
        _ => Err("a tar output without a file".into()),
    };
    written.map_err(|e| fail_export(progress, &v, &e))?;
    progress.borrow().line(&v, "sending tarball done");
    progress.borrow().done(&v);
    Ok(())
}

/// A file output's file, as buildx's client makes it: its directory with
/// `MkdirAll(dir, 0755)`, the file created or truncated (build/opt.go).
fn create_dest_file(path: &Path) -> Result<std::fs::File, String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o755);
        b.create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The `oci` and `docker` outputs to a file, stdout or (`oci` with `tar=false`) a
/// directory, each named by its `name` attribute or the build's tags, as buildx names
/// them (build/opt.go).
/// An image output's own layers, config and manifest, where it compresses or rewrites
/// them otherwise than the image (D85).
struct Variant {
    layers: Vec<Layer>,
    config: Vec<u8>,
    config_digest: Digest,
    manifest: Vec<u8>,
    manifest_digest: Digest,
}

fn write_image_outputs(
    outputs: &[buildflags::Output],
    store: &Store,
    image: &output::Made<'_>,
    variants: &[(usize, Variant)],
    tags: &[String],
    build_epoch: Option<i64>,
    progress: &RefCell<Progress>,
) -> Result<(), String> {
    // A platform's build of several writes its layout unseen: the build of them shows
    // its one export.
    let hushed = RefCell::new(Progress {
        quiet: true,
        next: 0,
        #[cfg(unix)]
        live: None,
    });
    let progress = if multi::active() { &hushed } else { progress };
    for (i, o) in outputs.iter().enumerate() {
        if !matches!(o.kind.as_str(), "oci" | "docker") || matches!(o.dest, buildflags::Dest::Store) {
            continue;
        }
        let own = variants
            .iter()
            .find(|(at, _)| *at == i)
            .map(|(_, v)| output::Made {
                config: &v.config,
                config_digest: &v.config_digest,
                manifest: &v.manifest,
                manifest_digest: &v.manifest_digest,
                layers: &v.layers,
                ..*image
            });
        let made = own.as_ref().unwrap_or(image);
        // Docker's types unless `oci-mediatypes` asks for OCI's, or (the `docker` exporter's
        // default) the output carries an attestation (D83).
        let docker = match o.attrs.get("oci-mediatypes") {
            Some(v) => !oci_types_attr(v)?,
            None => o.kind == "docker" && made.provenance.is_none(),
        };
        let v = progress
            .borrow_mut()
            .start(&format!("exporting to {} image format", o.kind));
        let fail = |e: String| fail_export(progress, &v, &e);
        let names: Vec<Reference> = match o.attrs.get("name") {
            Some(n) => n
                .split(',')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect(),
            None => tags.to_vec(),
        }
        .iter()
        .map(|n| Reference::parse(n.trim()).map_err(|e| fail(e.to_string())))
        .collect::<Result<_, _>>()?;
        let epoch = output::epoch(&o.attrs, build_epoch).map_err(fail)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let created = Time::from_unix(epoch.unwrap_or(now))
            .rfc3339_nano()
            .map_err(|e| fail(show(&e)))?;
        progress.borrow().line(&v, "exporting layers done");
        let manifest_digest = if docker {
            sha256(&export::docker_manifest(
                made.config,
                made.config_digest.to_string().as_bytes(),
                made.layers,
            ))
        } else {
            made.manifest_digest.clone()
        };
        progress
            .borrow()
            .line(&v, &format!("exporting manifest {manifest_digest} done"));
        progress
            .borrow()
            .line(&v, &format!("exporting config {} done", made.config_digest));
        match &o.dest {
            buildflags::Dest::Dir(dest) => {
                output::layout_dir(store, made, &names, &created, dest).map_err(fail)?;
            }
            buildflags::Dest::Stdout => {
                let out = std::io::BufWriter::new(std::io::stdout().lock());
                output::layout(store, made, docker, &names, &created, out).map_err(fail)?;
                progress.borrow().line(&v, "sending tarball done");
            }
            buildflags::Dest::File(path) => {
                let f = create_dest_file(path).map_err(fail)?;
                output::layout(store, made, docker, &names, &created, std::io::BufWriter::new(f))
                    .map_err(fail)?;
                progress.borrow().line(&v, "sending tarball done");
            }
            buildflags::Dest::Store => {}
        }
        progress.borrow().done(&v);
    }
    Ok(())
}

/// tonistiigi/units' `%.2f` of a byte count, as progressui prints one: decimal units,
/// whole bytes below a kilobyte.
fn units_bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
    let (mut i, mut base) = (0usize, 1u64);
    while i + 1 < UNITS.len() && n >= base.saturating_mul(1000) {
        base = base.saturating_mul(1000);
        i += 1;
    }
    if i == 0 {
        return format!("{n}B");
    }
    format!("{:.2}{}", n as f64 / base as f64, UNITS.get(i).unwrap_or(&"B"))
}

/// The named contexts as the frontend is given them, and the directories of the local
/// ones.
#[derive(Debug, Default)]
struct NamedContexts {
    contexts: BTreeMap<Vec<u8>, Vec<u8>>,
    keys: BTreeMap<Vec<u8>, Vec<u8>>,
    excludes: BTreeMap<Vec<u8>, Vec<Vec<u8>>>,
    locals: BTreeMap<Vec<u8>, PathBuf>,
    /// OCI layouts to import: each directory and the digest its context names.
    layouts: Vec<(PathBuf, Digest)>,
}

/// `--build-context`'s values as buildx's loadInputs hands them to the frontend
/// (build/opt.go, v0.37.1): a remote URL, `docker-image://` or `target:` as it is; else a
/// directory, sent as a local of the context's name (`_context` and `_dockerfile` for
/// those two), keyed by its base name, its `.dockerignore` read as the frontend reads it.
fn named_contexts(named: &BTreeMap<String, String>) -> Result<NamedContexts, String> {
    let mut out = NamedContexts::default();
    for (k, v) in named {
        let remote = ["http://", "https://", "git://", "github.com/", "git@"]
            .iter()
            .any(|p| v.starts_with(p));
        if remote || v.starts_with("docker-image://") || v.starts_with("target:") {
            out.contexts
                .insert(k.clone().into_bytes(), v.clone().into_bytes());
            continue;
        }
        if let Some(r) = parse_oci_layout(v)? {
            let digest = match &r.digest {
                Some(d) => d.clone(),
                None => resolve_layout_digest(&r.path, &r.tag).map_err(|e| {
                    format!("oci-layout reference {} could not be resolved: {e}", go_quote(v))
                })?,
            };
            // The layout's store, named for the frontend as buildx names it (a session
            // store's ID): here the order it is given in.
            let store = format!("layout{}", out.layouts.len());
            let mut spec = format!("oci-layout://{store}");
            if !r.tag.is_empty() {
                spec.push_str(&format!(":{}", r.tag));
            }
            spec.push_str(&format!("@{digest}"));
            out.contexts.insert(k.clone().into_bytes(), spec.into_bytes());
            out.layouts.push((PathBuf::from(&r.path), digest));
            continue;
        }
        let md = std::fs::metadata(v).map_err(|e| {
            format!(
                "failed to get build context {k}: stat {v}: {}",
                buildflags::os_error(&e)
            )
        })?;
        if !md.is_dir() {
            return Err(format!(
                "failed to get build context path {{{v} <nil>}}: not a directory"
            ));
        }
        let local = if k == "context" || k == "dockerfile" {
            format!("_{k}")
        } else {
            k.clone()
        };
        let dir = PathBuf::from(v);
        let key = std::path::absolute(&dir)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let ignore = read_if_present(&dir.join(".dockerignore"), ".dockerignore").map_err(|e| {
            format!("failed to build: failed to solve: failed to read dockerignore patterns: {e}")
        })?;
        out.excludes.insert(
            local.clone().into_bytes(),
            ignore
                .map(|t| shards_dockerfile::ignore::read_all(&t))
                .unwrap_or_default(),
        );
        out.contexts
            .insert(k.clone().into_bytes(), format!("local:{local}").into_bytes());
        out.keys.insert(k.clone().into_bytes(), key.into_bytes());
        out.locals.insert(local.into_bytes(), dir);
    }
    Ok(out)
}

fn go_quote(s: &str) -> String {
    shards_cmdline::go::quote(s)
}

/// An `oci-layout://` reference as buildx's ocilayout.Parse reads it: the directory, a
/// tag and a digest, `latest` the tag when neither is given.
struct LayoutRef {
    path: String,
    tag: String,
    digest: Option<Digest>,
}

fn parse_oci_layout(s: &str) -> Result<Option<LayoutRef>, String> {
    let Some(mut path) = s.strip_prefix("oci-layout://") else {
        return Ok(None);
    };
    let mut out = LayoutRef {
        path: String::new(),
        tag: String::new(),
        digest: None,
    };
    let digest_re =
        regex::Regex::new(r"^[A-Za-z][A-Za-z0-9]*(?:[-_+.][A-Za-z][A-Za-z0-9]*)*:[0-9a-fA-F]{32,}$")
            .map_err(|e| e.to_string())?;
    let tag_re = regex::Regex::new(r"^[\w][\w.-]{0,127}$").map_err(|e| e.to_string())?;
    if let Some(i) = path.rfind('@') {
        let after = path.get(i + 1..).unwrap_or_default();
        if digest_re.is_match(after) {
            out.digest = Some(Digest::parse(after).map_err(|e| e.to_string())?);
            path = path.get(..i).unwrap_or_default();
        }
    }
    if let Some(i) = path.rfind(':') {
        let windows_drive = i == 1
            && path.len() >= 3
            && matches!(path.as_bytes().get(2), Some(b'/' | b'\\'))
            && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
        let after = path.get(i + 1..).unwrap_or_default();
        if !windows_drive && tag_re.is_match(after) {
            out.tag = after.to_string();
            path = path.get(..i).unwrap_or_default();
        }
    }
    out.path = path.to_string();
    if out.tag.is_empty() && out.digest.is_none() {
        out.tag = "latest".into();
    }
    Ok(Some(out))
}

/// buildx's resolveDigest: the layout's index entry named `tag` (by its image name, then
/// its reference name), else its only entry.
fn resolve_layout_digest(dir: &str, tag: &str) -> Result<Digest, String> {
    let path = Path::new(dir).join("index.json");
    let bytes = std::fs::read(&path).map_err(|e| {
        format!(
            "could not read {}: open {}: {}",
            path.display(),
            path.display(),
            buildflags::os_error(&e)
        )
    })?;
    let index: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "could not unmarshal {} ({}): {e}",
            path.display(),
            go_quote(&String::from_utf8_lossy(&bytes))
        )
    })?;
    let manifests = index
        .get("manifests")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let annotation = |m: &serde_json::Value, k: &str| {
        m.get("annotations")
            .and_then(|a| a.get(k))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let found = manifests
        .iter()
        .find(|m| annotation(m, "io.containerd.image.name").as_deref() == Some(tag))
        .or_else(|| {
            manifests
                .iter()
                .find(|m| annotation(m, "org.opencontainers.image.ref.name").as_deref() == Some(tag))
        })
        .or_else(|| (manifests.len() == 1).then(|| manifests.first()).flatten())
        .ok_or("failed to resolve digest")?;
    let d = found
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    Digest::parse(d).map_err(|e| format!("invalid digest {d}: {e}"))
}

/// Imports the image `digest` names from the OCI layout in `dir` into the store, each
/// blob checked against its digest as it is stored: an index's manifest for this
/// platform, its config and layers. Returns that manifest's descriptor.
fn import_layout(store: &Store, dir: &Path, digest: &Digest) -> Result<Descriptor, String> {
    let blob = |d: &Digest| dir.join("blobs").join(d.algorithm().name()).join(d.hex());
    let put = |d: &Digest| -> Result<(), String> {
        if store.has(d) {
            return Ok(());
        }
        let path = blob(d);
        let size = std::fs::metadata(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        let mut f = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        store
            .ingest(d, size, &mut f)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(())
    };
    let read = |d: &Digest| -> Result<Vec<u8>, String> {
        put(d)?;
        let bytes = std::fs::read(store.blob_path(d)).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > oci::MAX_MANIFEST {
            return Err(format!("{d}: a manifest over {} bytes", oci::MAX_MANIFEST));
        }
        Ok(bytes)
    };
    let media = |bytes: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|v| v.get("mediaType").and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_else(|| oci::media::OCI_MANIFEST.to_string())
    };
    let mut d = digest.clone();
    let mut bytes = read(&d)?;
    let mut kind = media(&bytes);
    if let Document::Index(index) = oci::parse_document(&bytes, &kind).map_err(|e| e.to_string())? {
        let chosen = image_platform::select(&index, &image_platform::guest())
            .ok_or_else(|| format!("{digest}: no manifest for this platform in the layout"))?;
        d = chosen.digest().map_err(|e| e.to_string())?;
        bytes = read(&d)?;
        kind = media(&bytes);
    }
    let Document::Manifest(m) = oci::parse_document(&bytes, &kind).map_err(|e| e.to_string())? else {
        return Err(format!("{d}: an index inside an index"));
    };
    for part in std::iter::once(&m.config).chain(&m.layers) {
        put(&part.digest().map_err(|e| e.to_string())?)?;
    }
    Ok(Descriptor {
        media_type: kind,
        digest: d.to_string(),
        size: i64::try_from(bytes.len()).map_err(|e| e.to_string())?,
        platform: None,
        annotations: Default::default(),
    })
}

/// A guarded step's writes (D55, §9.2): what its new layers hold, read entry by entry, in
/// no domain but the one its own directive writes, if it is one. A removal is a write.
fn guard(
    meta: &shards_dockerfile::llb::Meta,
    inputs: &[exec::Ref],
    outs: &[exec::Ref],
    domains: &[plan::DomainDir],
    store: &Store,
) -> Result<(), String> {
    if domains.is_empty() || !meta.description.contains_key(plan::GUARD) {
        return Ok(());
    }
    let parts = |p: &[u8]| -> Vec<Vec<u8>> {
        p.split(|&b| b == b'/')
            .filter(|c| !c.is_empty() && *c != b".")
            .map(<[u8]>::to_vec)
            .collect()
    };
    let mut roots: Vec<(Vec<Vec<u8>>, usize)> = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        let dir = parts(&d.dir);
        let mut grants = dir.clone();
        if let Some(last) = grants.last_mut() {
            last.extend_from_slice(b".d");
        }
        roots.push((dir, i));
        roots.push((grants, i));
    }
    let domain_of = |p: &[Vec<u8>]| -> Option<usize> {
        roots
            .iter()
            .filter(|(r, _)| p.len() >= r.len() && p.get(..r.len()) == Some(r.as_slice()))
            .max_by_key(|(r, _)| r.len())
            .map(|&(_, d)| d)
    };
    let own = meta.description.get(plan::OWN).and_then(|o| domain_of(&parts(o)));
    let known: std::collections::HashSet<&[u8]> = inputs
        .iter()
        .flat_map(|r| r.layers.iter().map(|l| l.digest.as_slice()))
        .collect();
    for out in outs {
        for l in out.layers.iter().filter(|l| !known.contains(l.digest.as_slice())) {
            let digest = Digest::parse(&show(&l.digest)).map_err(|e| e.to_string())?;
            let file = std::fs::File::open(store.blob_path(&digest)).map_err(|e| format!("{digest}: {e}"))?;
            let mut r = shards_image::tar::Reader::seekable(std::io::BufReader::new(file))
                .map_err(|e| format!("{digest}: {e}"))?;
            // The directories above what changed come along in a layer; what is named is
            // the first thing written that is no directory, where there is one.
            let mut found: Option<(Vec<u8>, usize)> = None;
            while let Some(e) = r.next_entry().map_err(|e| format!("{digest}: {e}"))? {
                let mut p = parts(&e.path);
                if let Some(last) = p.last_mut()
                    && let Some(name) = last.strip_prefix(b".wh.")
                {
                    *last = name.to_vec();
                }
                let Some(d) = domain_of(&p) else { continue };
                if Some(d) == own {
                    continue;
                }
                let dir = matches!(e.kind, shards_image::tar::Type::Dir);
                if found.is_none() || !dir {
                    found = Some((p.join(&b"/"[..]), d));
                }
                if !dir {
                    break;
                }
            }
            if let Some((path, d)) = found {
                let dom = domains.get(d).map_or_else(String::new, |x| {
                    format!(
                        "{} {}",
                        if x.harness { "the harness" } else { "the agent" },
                        show(&x.name)
                    )
                });
                return Err(format!(
                    "it writes /{} in {dom}'s domain, which only that domain's own directives write (AGENTFILE_ARCH.md §9.2)",
                    show(&path)
                ));
            }
        }
    }
    Ok(())
}

/// The export's failure `why`, on its step and as the build's error.
fn fail_export(progress: &std::cell::RefCell<Progress>, v: &Vertex, why: &str) -> String {
    progress.borrow().error(v, why);
    format!("failed to build: failed to solve: {why}")
}

/// isSafeLocalDeleteDest (buildx build/opt.go): a directory under the working one, and not
/// it, which a local output's `mode=delete` may empty without `--allow buildx.local.delete`.
fn safe_delete_dest(dest: &Path) -> bool {
    let Ok(wd) = std::env::current_dir().and_then(|d| d.canonicalize()) else {
        return false;
    };
    // The path as far as it exists, resolved, and the rest as it is.
    let absolute = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        wd.join(dest)
    };
    let mut existing = absolute.clone();
    let mut rest = Vec::new();
    while existing.canonicalize().is_err() {
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_os_string) else {
            return false;
        };
        rest.push(name);
        if !existing.pop() {
            return false;
        }
    }
    let Ok(mut at) = existing.canonicalize() else {
        return false;
    };
    for name in rest.into_iter().rev() {
        at.push(name);
    }
    at.starts_with(&wd) && at != wd
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
/// `--call`'s method as buildx reads it (util/buildflags/callfunc.go `ParseCallFunc`,
/// `--check` its shorthand): none to build; for a check, whether its status is ignored and
/// its format. Methods shards does not serve yet are refused.
/// The frontend's options buildx sends for a build (build/opt.go toSolveOpt and
/// loadInputs, v0.37.1), which its provenance records as the request: build arguments
/// and labels, the target, the cache's and network's settings, hosts, `/dev/shm`, ulimits,
/// limits, the Dockerfile's name, and the named contexts.
fn buildx_attrs(
    parsed: &Parsed,
    filename: &str,
    add_hosts: &str,
    resources: &BTreeMap<String, String>,
    named: &NamedContexts,
) -> Result<BTreeMap<String, String>, String> {
    let mut a = BTreeMap::new();
    let cgroup = parsed.string("cgroup-parent");
    if !cgroup.is_empty() {
        a.insert("cgroup-parent".into(), cgroup.to_string());
    }
    let args = build_args(parsed.many("build-arg"), true);
    if args
        .get(b"BUILDKIT_MULTI_PLATFORM".as_slice())
        .is_some_and(|v| shards_cmdline::go::parse_bool(&show(v)).unwrap_or(false))
    {
        a.insert("multi-platform".into(), "true".into());
    }
    for (k, v) in &args {
        a.insert(format!("build-arg:{}", show(k)), show(v));
    }
    for (k, v) in build_args(parsed.many("label"), false) {
        a.insert(format!("label:{}", show(&k)), show(&v));
    }
    // The moby driver resolves images it has by default.
    a.insert(
        "image-resolve-mode".into(),
        if parsed.bool("pull") { "pull" } else { "local" }.into(),
    );
    let target = parsed.string("target");
    if !target.is_empty() {
        a.insert("target".into(), target.to_string());
    }
    let filters = parsed.many("no-cache-filter");
    if parsed.bool("no-cache") {
        a.insert("no-cache".into(), String::new());
    } else if !filters.is_empty() {
        a.insert("no-cache".into(), filters.join(","));
    }
    let platforms = parsed.many("platform");
    if !platforms.is_empty() {
        a.insert("platform".into(), platforms.join(","));
    }
    if let mode @ ("host" | "none") = parsed.string("network") {
        a.insert("force-network-mode".into(), mode.to_string());
    }
    if !add_hosts.is_empty() {
        a.insert("add-hosts".into(), add_hosts.to_string());
    }
    let shm = shards_dockerfile::dockerui::shm_size(parsed.string("shm-size")).unwrap_or(0);
    if shm > 0 {
        a.insert("shm-size".into(), shm.to_string());
    }
    let ulimits: Vec<String> = buildflags::ulimits(parsed.many("ulimit"))?
        .iter()
        .map(|u| format!("{}={}:{}", u.name, u.soft, u.hard))
        .collect();
    if !ulimits.is_empty() {
        a.insert("ulimit".into(), ulimits.join(","));
    }
    a.extend(resources.iter().map(|(k, v)| (k.clone(), v.clone())));
    a.insert("filename".into(), filename.to_string());
    for (k, v) in &named.contexts {
        a.insert(format!("context:{}", show(k)), show(v));
    }
    for (k, v) in &named.keys {
        a.insert(format!("sharedkey:localdir:{}", show(k)), show(v));
    }
    if !named.contexts.is_empty() {
        a.insert(
            "frontend.caps".into(),
            "moby.buildkit.frontend.contexts+forward".into(),
        );
    }
    Ok(a)
}

/// [`buildx_attrs`] of a build given `flags` alone: a named context's directory taken
/// as one, by its name, wherever it is.
#[cfg(test)]
fn buildx_attrs_of(flags: &[String]) -> Result<BTreeMap<String, String>, String> {
    // The attestations' own flags are none of the request's (FilterArgs).
    let mut argv: Vec<String> = Vec::new();
    let mut skip = false;
    for f in flags {
        if skip {
            skip = false;
            continue;
        }
        if f.starts_with("--provenance") || f.starts_with("--attest") || f.starts_with("--sbom") {
            skip = !f.contains('=');
            continue;
        }
        argv.push(f.clone());
    }
    argv.push(".".into());
    let parsed = match flags::parse(&BUILD, PATH, &argv, &buildflags::validate) {
        Outcome::Run(p) => p,
        _ => return Err(format!("{argv:?}: not parsed")),
    };
    let add_hosts = buildflags::add_hosts(parsed.many("add-host"), &|| Err("no gateway".into()))?;
    let resources = buildflags::resource_attrs(parsed.many("resource"))?;
    let mut named = NamedContexts::default();
    for v in parsed.many("build-context") {
        if let Some((k, dir)) = v.split_once('=') {
            named
                .contexts
                .insert(k.as_bytes().to_vec(), format!("local:{k}").into_bytes());
            let base = dir.rsplit('/').next().unwrap_or(dir);
            named.keys.insert(k.as_bytes().to_vec(), base.as_bytes().to_vec());
        }
    }
    let file = parsed.string("file");
    let filename = if file.is_empty() {
        "Dockerfile"
    } else {
        file.rsplit('/').next().unwrap_or(file)
    };
    buildx_attrs(&parsed, filename, &add_hosts, &resources, &named)
}

/// What a build asks of its provenance (D71, D72).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provenance {
    /// Nothing: BuildKit's default where an image is stored or pushed.
    Default,
    /// `--provenance=false`, or `--attest type=provenance,disabled=true`.
    Off,
    /// Its attributes: the builder's ID, whether it says it is reproducible, and whether it
    /// goes in an image alone (`inline-only`).
    Explicit {
        builder_id: String,
        reproducible: bool,
        inline_only: bool,
        /// `mode=max`: the build's definition, source map and layers too (D80).
        max: bool,
    },
}

/// `--attest`, `--provenance` and `--sbom`, as buildx reads them (toBuildOptions,
/// ParseAttests, ToMap) and BuildKit takes the provenance's attributes
/// (NewProvenanceCreator): `mode`, `version`, `builder-id`, `reproducible`,
/// `inline-only`; and the SBOM's (attestations.Parse, dockerui): its generator, and every
/// other attribute a parameter of the scanner's (D81). What shards does not make yet it
/// refuses, named.
fn provenance_of(parsed: &Parsed) -> Result<(Provenance, Option<sbom::Asked>), String> {
    let mut asked: Vec<String> = parsed.many("attest").to_vec();
    for kind in ["provenance", "sbom"] {
        let v = parsed.string(kind);
        if !v.is_empty() {
            asked.push(buildflags::canonicalize_attest(kind, v));
        }
    }
    let mut map = buildflags::attests_map(&buildflags::parse_attests(&asked)?);
    // `BUILDKIT_ATTEST_<TYPE>` build arguments, as BuildKit's `attestations.Parse` reads
    // them: the type lowercased, the value its attributes (no `type` among them); where a
    // flag asks for the same type, the flag (BuildKit takes whichever its map yields).
    for (k, v) in build_args(parsed.many("build-arg"), true) {
        let Some(kind) = show(&k).strip_prefix("BUILDKIT_ATTEST_").map(str::to_lowercase) else {
            continue;
        };
        if map.contains_key(&kind) {
            continue;
        }
        let v = show(&v);
        let off = shards_cmdline::go::csv_fields(v.as_bytes())
            .unwrap_or_default()
            .iter()
            .any(|f| show(f) == "disabled=true");
        map.insert(kind, (!off).then_some(v));
    }
    let mut out = Provenance::Default;
    let mut sbom_asked = None;
    for (kind, value) in map {
        match (kind.as_str(), value) {
            (_, None) if kind == "provenance" => out = Provenance::Off,
            (_, None) => {}
            ("sbom", Some(attrs)) => {
                let fields = shards_cmdline::go::csv_fields(attrs.as_bytes()).map_err(|e| show(&e))?;
                let mut generator = sbom::DEFAULT_GENERATOR.to_string();
                let mut params = BTreeMap::new();
                for f in fields {
                    let f = show(&f);
                    let (k, v) = f.split_once('=').unwrap_or((f.as_str(), ""));
                    if k == "generator" {
                        generator = v.to_string();
                    } else {
                        params.insert(k.to_string(), v.to_string());
                    }
                }
                let named = Reference::parse(&generator)
                    .map_err(|e| format!("failed to parse sbom scanner {generator}: {e}"))?;
                sbom_asked = Some(sbom::Asked {
                    generator: named.to_string(),
                    params,
                });
            }
            ("provenance", Some(attrs)) => {
                let fields = shards_cmdline::go::csv_fields(attrs.as_bytes()).map_err(|e| show(&e))?;
                let (mut builder_id, mut reproducible, mut inline_only, mut max) =
                    (String::new(), false, false, false);
                for f in fields {
                    let f = show(&f);
                    let Some((k, v)) = f.split_once('=') else { continue };
                    match k {
                        "mode" => match v {
                            "min" => max = false,
                            "max" | "full" => max = true,
                            _ => return Err(format!("invalid mode {}", go_quote(v))),
                        },
                        "version" => match v {
                            "v1" => {}
                            "v0.2" => {
                                return Err("provenance version=v0.2 is not supported by shards yet".into());
                            }
                            _ => return Err(format!("invalid provenance SLSA version: {v}")),
                        },
                        "builder-id" => builder_id = v.to_string(),
                        "reproducible" => {
                            reproducible = shards_cmdline::go::parse_bool(v).map_err(|e| {
                                format!("failed to parse reproducible flag {}: {e}", go_quote(v))
                            })?;
                        }
                        "inline-only" => inline_only = shards_cmdline::go::parse_bool(v).unwrap_or(false),
                        _ => {}
                    }
                }
                out = Provenance::Explicit {
                    builder_id,
                    reproducible,
                    inline_only,
                    max,
                };
            }
            (other, Some(_)) => return Err(format!("attestation type {other} is not supported by shards")),
        }
    }
    Ok((out, sbom_asked))
}

/// Whether an image the build stores or pushes carries its provenance, as buildx asks
/// for it by default (`attest:provenance=mode=min,inline-only=true`), unless
/// BUILDX_NO_DEFAULT_ATTESTATIONS says not.
fn attests(env: &dyn Fn(&str) -> Option<String>) -> Result<bool, String> {
    match env("BUILDX_NO_DEFAULT_ATTESTATIONS") {
        None => Ok(true),
        Some(v) => shards_cmdline::go::parse_bool(&v)
            .map(|off| !off)
            .map_err(|e| format!("invalid BUILDX_NO_DEFAULT_ATTESTATIONS: {e}")),
    }
}

/// `urlutil.RedactCredentials` (BuildKit v0.28.1): a URL's user and password each said as
/// `xxxxx` where given; what is no URL (scp's `user@host:path`) as it is.
fn redact_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let (user, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u, Some(p)),
        None => (userinfo, None),
    };
    let masked = match (user.is_empty(), password) {
        (false, Some(_)) => "xxxxx:xxxxx".to_string(),
        (false, None) => "xxxxx".to_string(),
        (true, Some(_)) => ":xxxxx".to_string(),
        (true, None) => String::new(),
    };
    if masked.is_empty() {
        format!("{scheme}://@{host}{path}")
    } else {
        format!("{scheme}://{masked}@{host}{path}")
    }
}

#[cfg(test)]
mod redact_tests {
    #[test]
    fn credentials_are_redacted_as_buildkit_redacts_them() {
        let r = super::redact_credentials;
        assert_eq!(
            r("https://user:pw@host.tld/path.git"),
            "https://xxxxx:xxxxx@host.tld/path.git"
        );
        assert_eq!(
            r("ssh://git@github.com/o/r.git"),
            "ssh://xxxxx@github.com/o/r.git"
        );
        assert_eq!(r("https://:pw@h/p"), "https://:xxxxx@h/p");
        assert_eq!(r("https://h/p?a=b@c"), "https://h/p?a=b@c");
        assert_eq!(r("git@github.com:o/r.git"), "git@github.com:o/r.git");
    }
}

/// Now, as seconds and nanoseconds since 1970.
fn unix_now() -> (i64, u32) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (i64::try_from(d.as_secs()).unwrap_or(i64::MAX), d.subsec_nanos())
}

/// What `--call` (or `--check`) asks of the frontend instead of a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// The build's checks; `json` for `format=json`; with `ignorestatus`, exit 0
    /// whatever they found.
    Check {
        json: bool,
        ignore_status: bool,
    },
    /// A subrequest: the target's outline, the targets, or the subrequests; `json` for
    /// `format=json`.
    Outline {
        json: bool,
    },
    Targets {
        json: bool,
    },
    Describe {
        json: bool,
    },
}

/// `--call`, as buildx v0.37.1 reads it (util/buildflags/callfunc.go ParseCallFunc).
fn call_of(parsed: &Parsed) -> Result<Option<Call>, String> {
    let given = if parsed.bool("check") {
        "check"
    } else {
        parsed.string("call")
    };
    if given.is_empty() {
        return Ok(None);
    }
    let fields = shards_cmdline::go::csv_fields(given.as_bytes()).map_err(|e| show(&e))?;
    let (mut name, mut format, mut ignore) = (String::new(), String::new(), false);
    for f in fields {
        let f = show(&f);
        match f.split_once('=') {
            Some(("format", v)) => format = v.to_string(),
            Some(("ignorestatus", v)) => {
                ignore = shards_cmdline::go::parse_bool(v)
                    .map_err(|e| format!("invalid ignorestatus print value: {v}: {e}"))?;
            }
            Some(_) => return Err(format!("invalid print field: {f}")),
            None if !name.is_empty() => return Err(format!("invalid print value: {given}")),
            None => name = f,
        }
    }
    // printValue prints result.json for json, and the text for any other format.
    let json = format == "json";
    match name.as_str() {
        "build" => Ok(None),
        "check" | "lint" => Ok(Some(Call::Check {
            json,
            ignore_status: ignore,
        })),
        "outline" => Ok(Some(Call::Outline { json })),
        "targets" => Ok(Some(Call::Targets { json })),
        "subrequests.describe" => Ok(Some(Call::Describe { json })),
        other => Err(format!("--call={other} is not supported by shards yet")),
    }
}

/// The build's checks as BuildKit's lint subrequest prints them (frontend/subrequests/lint
/// `LintResults.PrintTo`): by line, each its rule, URL, message and lines.
fn lint_text(warnings: &[shards_dockerfile::lint::Warning], file: &str, text: &[u8]) -> String {
    let mut sorted: Vec<&shards_dockerfile::lint::Warning> = warnings.iter().collect();
    sorted.sort_by(|a, b| match (a.location.first(), b.location.first()) {
        (None, None) => a.rule.cmp(b.rule),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.0.cmp(&y.0),
    });
    let mut out = String::new();
    for w in sorted {
        out.push_str(&format!("\nWARNING: {}", w.rule));
        if !w.url.is_empty() {
            out.push_str(&format!(" - {}", w.url));
        }
        out.push_str(&format!("\n{}\n", show(&w.message)));
        if !w.location.is_empty() {
            out.push_str(&excerpt(file, text, &w.location));
        }
    }
    out
}

fn print_warnings(
    warnings: &[shards_dockerfile::lint::Warning],
    quiet: bool,
    debug: bool,
    file: &str,
    text: &[u8],
) {
    if warnings.is_empty() || quiet {
        return;
    }
    let count = if warnings.len() == 1 {
        "1 warning found".to_string()
    } else {
        format!("{} warnings found", warnings.len())
    };
    // With --debug, each in full (commands/build.go printWarnings): its description,
    // its URL, and its lines.
    let hint = if debug {
        ""
    } else {
        " (use shards --debug to expand)"
    };
    let mut out = format!("\n {YELLOW}{count}{hint}:\n{RESET}");
    for w in warnings {
        let line = w.location.first().map_or(0, |r| r.0);
        let mut short = format!("{}: {}", w.rule, show(&w.message));
        if line > 0 {
            short.push_str(&format!(" (line {line})"));
        }
        out.push_str(&format!(" - {short}\n"));
        if !debug {
            continue;
        }
        out.push_str(&format!("{}\n", w.description));
        if !w.url.is_empty() {
            out.push_str(&format!("More info: {}\n", w.url));
        }
        if !w.location.is_empty() {
            out.push_str(&excerpt(file, text, &w.location));
        }
        out.push('\n');
    }
    let _ = write!(std::io::stderr(), "{out}");
}

/// An environment variable's value, its bytes as they are.
fn os_bytes(v: std::ffi::OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStringExt::into_vec(v)
    }
    #[cfg(not(unix))]
    {
        v.to_string_lossy().into_owned().into_bytes()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Byte counts as tonistiigi/units' `%#g` writes them (measured, go1.26 in BuildKit's
    /// module).
    #[test]
    fn byte_counts_read_as_buildkits() {
        for (b, want) in [
            (2 << 20, "2MiB"),
            (200 << 10, "200KiB"),
            (1000, "1000B"),
            (1024, "1KiB"),
            (1536, "1.5KiB"),
            (1 << 30, "1GiB"),
            (0, "0B"),
            (3 << 19, "1.5MiB"),
            (-(2 << 20), "-2MiB"),
        ] {
            assert_eq!(units(b), want);
        }
    }

    /// A stream passes as much as its limit lets, a notice where clipping begins and
    /// nothing after; the tail keeps what came from the clipped write on; within its first
    /// second a stream may print its speed's worth.
    #[test]
    fn streams_are_clipped_as_buildkitd_clips_them() {
        let size = LogLimits { size: 10, speed: -1 };
        let mut c = Clip::new();
        assert_eq!(c.write(b"abcdef", size), b"abcdef");
        assert_eq!(
            c.write(b"ghijkl", size),
            b"ghij\n[output clipped, log limit 10B reached]\n"
        );
        assert_eq!(c.write(b"mno", size), b"");
        // From the write that was clipped on: buildkitd makes the tail then.
        assert_eq!(c.tail.as_ref().unwrap().bytes(), b"ghijklmno");
        let speed = LogLimits {
            size: 2 << 20,
            speed: 5,
        };
        // Begun a moment ago: the clock may not have moved since `new`, and no second
        // begun is no budget, as in buildkitd.
        let mut c = Clip::new();
        c.began = Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            c.write(b"12345678", speed),
            b"12345\n[output clipped, log limit 5B/s reached]\n"
        );
    }

    /// The tail keeps the last 256 KiB, in order, however written.
    #[test]
    fn the_tail_keeps_the_last_bytes_in_order() {
        let mut t = Tail {
            data: vec![0; Tail::SIZE],
            at: 0,
            full: false,
        };
        let all: Vec<u8> = (0..Tail::SIZE * 2 + 123).map(|i| (i % 251) as u8).collect();
        for chunk in all.chunks(7777) {
            t.write(chunk);
        }
        assert_eq!(t.bytes(), all.get(all.len() - Tail::SIZE..).unwrap());
        t.write(&all);
        assert_eq!(t.bytes(), all.get(all.len() - Tail::SIZE..).unwrap());
    }

    /// Lines are stamped when they begin, to 3 places, 2 past 10 s and 1 past 100; a
    /// line not ended is printed as far as it goes and continued as the rest comes.
    #[test]
    fn build_refs_are_buildkits_ids() {
        let r = build_ref().unwrap();
        assert_eq!(r.len(), 25, "{r}");
        assert!(
            r.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
            "{r}"
        );
        assert_ne!(r, build_ref().unwrap());
    }

    #[test]
    fn metadata_is_written_as_buildx_writes_it() {
        let manifest = Digest::parse(&format!("sha256:{}", "b".repeat(64))).unwrap();
        // As Go's MarshalIndent writes buildx's map: keys sorted, two spaces, the
        // descriptor's fields in its own order.
        assert_eq!(
            metadata(
                "r",
                Some((&manifest, 481, &["docker.io/library/app:1".to_string()])),
                &BTreeMap::new(),
                &[],
                oci::media::OCI_MANIFEST,
            ),
            format!(
                "{{\n  \"buildx.build.ref\": \"shards/shards/r\",\n  \"containerimage.descriptor\": {{\n    \"mediaType\": \"application/vnd.oci.image.manifest.v1+json\",\n    \"digest\": \"{manifest}\",\n    \"size\": 481\n  }},\n  \"containerimage.digest\": \"{manifest}\",\n  \"image.name\": \"docker.io/library/app:1\"\n}}"
            )
        );
        assert_eq!(
            metadata("r", None, &BTreeMap::new(), &[], oci::media::OCI_MANIFEST),
            "{\n  \"buildx.build.ref\": \"shards/shards/r\"\n}"
        );
    }

    #[test]
    fn lines_print_as_progressui_prints_them() {
        let mut log = StepLog::new(LogLimits { size: -1, speed: -1 });
        let v = Vertex {
            index: 7,
            started: Instant::now(),
        };
        let shown = |log: &mut StepLog, v: &Vertex, b: &[u8]| String::from_utf8(log.show(v, b)).unwrap();
        let first = shown(&mut log, &v, b"ab");
        assert!(
            first.starts_with("#7 0.0") && first.ends_with(" ab") && first.len() == "#7 0.000 ab".len(),
            "{first}"
        );
        assert_eq!(shown(&mut log, &v, b"c\n"), "c\n");
        let next = shown(&mut log, &v, b"d\ne");
        assert!(
            next.starts_with("#7 0.0") && next.contains(" d\n#7 0.0") && next.ends_with(" e"),
            "{next}"
        );
        for (ago, places) in [(12, 2), (150, 1)] {
            let v = Vertex {
                index: 1,
                started: Instant::now()
                    .checked_sub(std::time::Duration::from_secs(ago))
                    .unwrap(),
            };
            let mut log = StepLog::new(LogLimits { size: -1, speed: -1 });
            let line = shown(&mut log, &v, b"x\n");
            let stamp = line.split(' ').nth(1).unwrap();
            assert_eq!(stamp.split('.').nth(1).unwrap().len(), places, "{line}");
        }
    }
}
