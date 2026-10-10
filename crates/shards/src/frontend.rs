//! `shards frontend`: shards as the BuildKit frontend a `# syntax=` line names (D113), so
//! that `docker buildx build` builds Dockerfiles and Agentfiles with plain Docker and
//! BuildKit. BuildKit runs it from its own image (FROM scratch, `shards` alone) with no
//! network: its stdin and stdout the gateway's gRPC connection, its options in
//! BUILDKIT_FRONTEND_OPT_* (frontend/gateway/gateway.go). It asks and answers as
//! docker/dockerfile:1 does (dockerfile/1.27.1's builder.Build over dockerui and
//! grpcclient), each call measured in shards-dind (scripts/gateway/capture): the
//! Dockerfile read from the client, planned by shards' planner (held to Dockerfile2LLB),
//! solved, and returned with its image config; an error returned with the source it is
//! in, from which the client prints its excerpt.
//!
//! Where it does better, deliberately:
//! - nothing depends on map order: which unsupported capability a refusal names, and
//!   each map a definition carries, come in one order;
//! - an error carries no Go stack trace, which shards, not Go, has none of.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::ExitCode;

use shards_dockerfile::lint::Warning;
use shards_dockerfile::llb::{Definition, Input, Meta, Op, OpKind};
use shards_dockerfile::parser::Dialect;
use shards_dockerfile::pb::{self, Carried, Marshalled, SourceInfo};
use shards_dockerfile::plan::{self, EpochSource, MainContext, Options, Resolved, Resolver};
use shards_dockerfile::platform::{self, Platform};
use shards_gateway::gateway::{self as gw, Client, Pong, Ref, Returned, RpcStatus, Solve};
use shards_gateway::grpc;

/// The most a file the frontend reads may be: dockerui's maxFileSize, containerd's
/// DefaultMaxRecvMsgSize, 16 MiB.
const MAX_FILE: i64 = 16 << 20;

/// What this frontend can do of what a client may ask (builder/caps.go's enabledCaps, but
/// `moby.buildkit.frontend.inputs`, which it does not read yet): the label its image
/// carries, so that BuildKit refuses a build that needs more before it runs it.
pub(crate) const CAPS: [&str; 4] = [
    "moby.buildkit.frontend.subrequests",
    "moby.buildkit.frontend.contexts",
    "moby.buildkit.frontend.gitquerystring",
    "moby.buildkit.frontend.contexts.zstd",
];

/// The build's error as the frontend returns it (grpcerrors.ToGRPC): its code, message
/// and details, each a type URL and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    code: i32,
    message: String,
    details: Vec<(String, Vec<u8>)>,
}

/// gRPC's Unknown, which ToGRPC gives an error that names no code.
const UNKNOWN: i32 = 2;
/// gRPC's Unimplemented.
const UNIMPLEMENTED: i32 = 12;
/// errdefs.Source's detail, which the client prints the excerpt of a file from.
const SOURCE_DETAIL: &str = "github.com/moby/buildkit/errdefs.Source+json";
/// errdefs.FrontendCap's, of a frontend capability a frontend lacks.
const FRONTEND_CAP_DETAIL: &str = "github.com/moby/buildkit/errdefs.FrontendCap+json";
/// errdefs.Subrequest's, of a subrequest a frontend does not answer.
const SUBREQUEST_DETAIL: &str = "github.com/moby/buildkit/errdefs.Subrequest+json";

impl Failure {
    fn new(message: impl Into<String>) -> Failure {
        Failure {
            code: UNKNOWN,
            message: message.into(),
            details: Vec::new(),
        }
    }

    /// A BuildKit call's error: its status as BuildKit gave it, details and all. A
    /// frontend's wrapping of it is not said: ToGRPC keeps a status's own message.
    fn call(e: grpc::Error) -> Failure {
        match e {
            grpc::Error::Status(s) => Failure {
                code: i32::try_from(s.code).unwrap_or(UNKNOWN),
                message: s.message,
                details: RpcStatus::read(&s.details)
                    .map(|st| st.details)
                    .unwrap_or_default(),
            },
            grpc::Error::Transport(t) => Failure::new(t),
        }
    }

    fn status(&self) -> RpcStatus {
        RpcStatus {
            code: self.code,
            message: self.message.clone(),
            details: self.details.clone(),
        }
    }
}

/// What BuildKit gave the frontend's process (grpcclient's opts, sessionID, workers and
/// product).
#[derive(Debug, Clone, Default)]
struct Env {
    opts: BTreeMap<String, String>,
    session: String,
    /// Each worker's platforms, the first worker's first the build's.
    workers: Vec<Vec<Platform>>,
    product: String,
}

impl Env {
    /// The environment's, or none where BuildKit gave no session: no gateway is there.
    fn read(vars: impl Iterator<Item = (OsString, OsString)>) -> Option<Env> {
        let mut env = Env::default();
        let mut session = None;
        for (k, v) in vars {
            let (k, v) = (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned());
            if k.starts_with("BUILDKIT_FRONTEND_OPT_") {
                if let Some((key, value)) = v.split_once('=') {
                    env.opts.insert(key.to_string(), value.to_string());
                }
            } else if k == "BUILDKIT_SESSION_ID" {
                session = Some(v);
            } else if k == "BUILDKIT_WORKERS" {
                env.workers = workers(&v);
            } else if k == "BUILDKIT_EXPORTEDPRODUCT" {
                env.product = v;
            }
        }
        env.session = session?;
        Some(env)
    }
}

/// BUILDKIT_WORKERS' platforms, as grpcclient reads its JSON (none where it does not
/// parse).
fn workers(json: &str) -> Vec<Vec<Platform>> {
    let Ok(serde_json::Value::Array(list)) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    list.iter()
        .map(|w| {
            w.get("platforms")
                .and_then(|p| p.as_array())
                .map(|ps| ps.iter().map(platform_of_json).collect())
                .unwrap_or_default()
        })
        .collect()
}

fn platform_of_json(p: &serde_json::Value) -> Platform {
    let s = |k: &str| {
        p.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .as_bytes()
            .to_vec()
    };
    Platform {
        os: s("os"),
        architecture: s("architecture"),
        variant: s("variant"),
        os_version: s("os.version"),
        os_features: p
            .get("os.features")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str())
                    .map(|f| f.as_bytes().to_vec())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// This process's platform as containerd's DefaultSpec says it, normalized: Linux, its
/// architecture, and on arm64 the variant /proc/cpuinfo's "CPU architecture: 8" gives,
/// v8, which normalizing leaves out.
fn default_spec() -> Platform {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    Platform::new("linux", arch)
}

/// The capabilities BuildKit said it has (Ping), or the ones grpcclient takes for one that
/// says none (defaultCaps, defaultLLBCaps): each enabled or disabled with its message.
struct Caps {
    frontend: BTreeMap<String, (bool, String)>,
    llb: BTreeMap<String, (bool, String)>,
}

impl Caps {
    fn of(pong: &Pong) -> Caps {
        let set = |list: &[gw::Cap], defaults: &[&str]| -> BTreeMap<String, (bool, String)> {
            if list.is_empty() {
                return defaults
                    .iter()
                    .map(|c| ((*c).to_string(), (true, String::new())))
                    .collect();
            }
            list.iter()
                .map(|c| (c.id.clone(), (c.enabled, c.disabled_reason_msg.clone())))
                .collect()
        };
        Caps {
            frontend: set(
                &pong.frontend_caps,
                &["solve.base", "solve.inlinereturn", "resolveimage", "readfile"],
            ),
            llb: set(
                &pong.llb_caps,
                &[
                    "source.image",
                    "source.local",
                    "source.local.unique",
                    "source.local.sessionid",
                    "source.local.includepatterns",
                    "source.local.followpaths",
                    "source.local.excludepatterns",
                    "source.local.sharedkeyhint",
                    "source.git",
                    "source.git.keepgitdir",
                    "source.git.fullurl",
                    "source.http",
                    "source.http.checksum",
                    "source.http.perm",
                    "soruce.http.uidgid",
                    "source.buildop.llbfilename",
                    "exec.meta.base",
                    "exec.meta.proxyenv",
                    "exec.mount.bind",
                    "exec.mount.cache",
                    "exec.mount.cache.sharing",
                    "exec.mount.selector",
                    "exec.mount.tmpfs",
                    "exec.mount.secret",
                    "constraints",
                    "platform",
                    "meta.ignorecache",
                    "meta.description",
                    "meta.exportcache",
                ],
            ),
        }
    }

    fn has_frontend(&self, id: &str) -> bool {
        self.frontend.get(id).is_some_and(|c| c.0)
    }

    /// `llbCaps.Supports(id)`: none where the worker has it, else its refusal.
    fn llb_refusal(&self, id: &str, product: &str) -> Option<String> {
        use shards_dockerfile::caps::{Lacks, refusal};
        match self.llb.get(id) {
            Some((true, _)) => None,
            Some((false, reason)) => Some(refusal(id, Lacks::Disabled(reason), product)),
            None => Some(refusal(id, Lacks::Absent, product)),
        }
    }

    /// The worker's capabilities, for the planner.
    fn llb_enabled(&self) -> BTreeSet<Vec<u8>> {
        self.llb
            .iter()
            .filter(|(_, (on, _))| *on)
            .map(|(id, _)| id.as_bytes().to_vec())
            .collect()
    }
}

/// A cache to import (CacheOptionsEntry): its type and attributes.
type CacheImport = (String, BTreeMap<String, String>);

/// A ResolveImageConfig request: the name, the platform, the step's name.
type ImageRequest = (Vec<u8>, Vec<u8>, Vec<u8>);

/// What the build is asked, as dockerui's Client.init reads the options.
struct Config {
    options: Options,
    target_platforms: Vec<Platform>,
    multi_platform: bool,
    cache_imports: Vec<CacheImport>,
    filename: String,
    dockerfile_local: String,
    force_local_dockerfile: bool,
}

fn filter(opts: &BTreeMap<String, String>, prefix: &str) -> BTreeMap<Vec<u8>, Vec<u8>> {
    opts.iter()
        .filter_map(|(k, v)| {
            k.strip_prefix(prefix)
                .map(|n| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
        })
        .collect()
}

/// dockerui's validateMinCaps, then Client.init: each option read in its order, its errors
/// in its words. `opts` takes what init writes back to them.
fn config(opts: &mut BTreeMap<String, String>, env: &Env, caps: &Caps) -> Result<Config, Failure> {
    if let Some(e) = caps.llb_refusal("file.base", &env.product) {
        return Err(Failure::new(format!("needs BuildKit 0.5 or later: {e}")));
    }
    if opts.get("override-copy-image").is_some_and(|v| !v.is_empty()) {
        return Err(Failure::new(
            "support for \"override-copy-image\" was removed in BuildKit 0.11",
        ));
    }
    if let Some(v) = opts.get("build-arg:BUILDKIT_DISABLE_FILEOP")
        && shards_image::go::parse_bool(v.as_bytes()) == Some(true)
    {
        return Err(Failure::new(
            "support for \"BUILDKIT_DISABLE_FILEOP\" build-arg was removed in BuildKit 0.11",
        ));
    }
    let build_platform = env
        .workers
        .first()
        .and_then(|w| w.first())
        .cloned()
        .unwrap_or_else(|| platform::normalize(&default_spec()));
    let mut target_platforms = Vec::new();
    if let Some(v) = opts.get("platform").filter(|v| !v.is_empty()) {
        for p in v.split(',') {
            let parsed = platform::parse(p.as_bytes(), &default_spec()).map_err(|e| {
                Failure::new(format!(
                    "failed to parse target platform {p}: {}",
                    String::from_utf8_lossy(&e)
                ))
            })?;
            target_platforms.push(platform::normalize(&parsed));
        }
    }
    let resolve_mode =
        shards_dockerfile::dockerui::resolve_mode(opt(opts, "image-resolve-mode")).map_err(Failure::new)?;
    let extra_hosts = shards_dockerfile::dockerui::extra_hosts(opt(opts, "add-hosts"))
        .map_err(|e| Failure::new(format!("failed to parse additional hosts: {e}")))?;
    let shm_size = shards_dockerfile::dockerui::shm_size(opt(opts, "shm-size"))
        .map_err(|e| Failure::new(format!("failed to parse shm size: {e}")))?;
    let ulimits =
        ulimits(opt(opts, "ulimit")).map_err(|e| Failure::new(format!("failed to parse ulimit: {e}")))?;
    let linux_resources = shards_dockerfile::dockerui::linux_resources(opts)
        .map_err(|e| Failure::new(format!("failed to parse resource limits: {e}")))?;
    let network_mode =
        shards_dockerfile::dockerui::net_mode(opt(opts, "force-network-mode")).map_err(Failure::new)?;
    let no_cache = opts.get("no-cache").map(|v| {
        if v.is_empty() {
            Vec::new()
        } else {
            v.split(',').map(|n| n.as_bytes().to_vec()).collect()
        }
    });
    let mut multi_platform = target_platforms.len() > 1;
    if let Some(v) = opts
        .get("build-arg:BUILDKIT_MULTI_PLATFORM")
        .filter(|v| !v.is_empty())
        .cloned()
    {
        opts.insert("multi-platform".into(), v);
    }
    if let Some(v) = opts.get("multi-platform").filter(|v| !v.is_empty()) {
        let b = shards_image::go::parse_bool(v.as_bytes())
            .ok_or_else(|| Failure::new(format!("invalid boolean value for multi-platform: {v}")))?;
        if !b && multi_platform {
            return Err(Failure::new(
                "conflicting config: returning multiple target platforms is not allowed",
            ));
        }
        multi_platform = b;
    }
    let cache_imports = cache_imports(opts)?;
    // attest:sbom runs a scanner over the result and returns its attestation, which this
    // frontend does not make yet: refused, never left out of an image that asked for it.
    if opts.contains_key("attest:sbom") {
        return Err(Failure::new(
            "shards' frontend makes no SBOM attestations yet (attest:sbom): build without --sbom, or with docker/dockerfile",
        ));
    }
    if let Some(v) = opts
        .get("build-arg:BUILDKIT_SANDBOX_HOSTNAME")
        .filter(|v| !v.is_empty())
        .cloned()
    {
        opts.insert("hostname".into(), v);
    }
    if let Some(v) = opts.get("build-arg:BUILDKIT_DOCKERFILE_CHECK") {
        shards_dockerfile::lint::parse_options(v.as_bytes()).map_err(|e| {
            Failure::new(format!(
                "failed to parse build-arg:BUILDKIT_DOCKERFILE_CHECK: {}",
                String::from_utf8_lossy(&e)
            ))
        })?;
    }
    let mut git_advice = false;
    if let Some(v) = opts
        .get("build-arg:BUILDKIT_GIT_ADVICE")
        .filter(|v| !v.is_empty())
    {
        git_advice = shards_image::go::parse_bool(v.as_bytes()).ok_or_else(|| {
            Failure::new(format!(
                "failed to parse build-arg:BUILDKIT_GIT_ADVICE: strconv.ParseBool: parsing {}: invalid syntax",
                go_quote(v)
            ))
        })?;
    }
    let filename = opts
        .get("filename")
        .cloned()
        .unwrap_or_else(|| "Dockerfile".into());
    // initContext: the context a Git or HTTP(S) URL is, in the option `contextkey` names
    // (`context` by default); BUILDKIT_CONTEXT_KEEP_GIT_DIR where it is a boolean.
    let context_key = opts
        .get("contextkey")
        .cloned()
        .unwrap_or_else(|| "context".into());
    let context_url = opt(opts, &context_key).to_string();
    let main_context = match shards_dockerfile::git::parse_git_ref(context_url.as_bytes()) {
        shards_dockerfile::git::Parsed::Git(_) => MainContext::Git {
            url: context_url.as_bytes().to_vec(),
            keep_git_dir: shards_image::go::parse_bool(
                opt(opts, "build-arg:BUILDKIT_CONTEXT_KEEP_GIT_DIR").as_bytes(),
            )
            .unwrap_or(false),
        },
        shards_dockerfile::git::Parsed::BadGit(e) => {
            return Err(Failure::new(String::from_utf8_lossy(&e)));
        }
        shards_dockerfile::git::Parsed::NotGit
            if context_url.starts_with("http://") || context_url.starts_with("https://") =>
        {
            MainContext::Http {
                url: context_url.as_bytes().to_vec(),
                archive: false,
            }
        }
        shards_dockerfile::git::Parsed::NotGit => MainContext::Local,
    };
    let (dockerfile_local, force_local_dockerfile) = match opts.get("dockerfilekey") {
        Some(k) => (k.clone(), true),
        None => ("dockerfile".to_string(), false),
    };
    let options = Options {
        target_platform: build_platform.clone(),
        implicit_target: true,
        build_platforms: vec![build_platform],
        build_args: filter(opts, "build-arg:"),
        target: opt(opts, "target").as_bytes().to_vec(),
        all_stages: false,
        labels: filter(opts, "label:"),
        hostname: opt(opts, "hostname").as_bytes().to_vec(),
        ulimits,
        multi_platform,
        context_id: env.session.as_bytes().to_vec(),
        excludes: Vec::new(),
        dialect: dialect_of(&filename),
        contexts: filter(opts, "context:"),
        context_keys: filter(opts, "sharedkey:localdir:"),
        context_excludes: BTreeMap::new(),
        no_cache,
        extra_hosts,
        shm_size,
        cgroup_parent: opt(opts, "cgroup-parent").as_bytes().to_vec(),
        linux_resources,
        network_mode,
        image_resolve_mode: resolve_mode.to_vec(),
        main_context,
        context_subdir: opts.get("contextsubdir").map(|s| s.as_bytes().to_vec()),
        git_advice,
        session: env.session.as_bytes().to_vec(),
        local_sessions: filter(opts, "local-sessionid:"),
        llb_caps: Some(caps.llb_enabled()),
        cmdline: opts.get("cmdline").map(|s| s.as_bytes().to_vec()),
    };
    Ok(Config {
        options,
        target_platforms,
        multi_platform,
        cache_imports,
        filename,
        dockerfile_local,
        force_local_dockerfile,
    })
}

/// Option `k`, empty where absent.
fn opt<'a>(opts: &'a BTreeMap<String, String>, k: &str) -> &'a str {
    opts.get(k).map(String::as_str).unwrap_or_default()
}

/// Go's %q of a string: its quoting.
fn go_quote(s: &str) -> String {
    shards_image::go::quote(s.as_bytes())
}

/// An Agentfile by its name (D35), as `shards build` tells one: `Agentfile`, `*.agentfile`
/// or `agentfile.*`, any case.
fn dialect_of(name: &str) -> Dialect {
    let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    if base == "agentfile" || base.ends_with(".agentfile") || base.starts_with("agentfile.") {
        Dialect::Agentfile
    } else {
        Dialect::Dockerfile
    }
}

/// parseUlimits: CSV fields, each go-units' ulimit.
fn ulimits(v: &str) -> Result<Vec<shards_dockerfile::llb::Ulimit>, String> {
    if v.is_empty() {
        return Ok(Vec::new());
    }
    let fields =
        shards_cmdline::go::csv_fields(v.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    let mut out = Vec::with_capacity(fields.len());
    for f in fields {
        let u = shards_cmdline::buildflags::parse_ulimit(&String::from_utf8_lossy(&f))?;
        out.push(shards_dockerfile::llb::Ulimit {
            name: u.name.into_bytes(),
            soft: u.soft,
            hard: u.hard,
        });
    }
    Ok(out)
}

/// The caches the build imports: `cache-imports`' JSON entries, then `cache-from`'s
/// registry references (dockerui's init).
fn cache_imports(opts: &BTreeMap<String, String>) -> Result<Vec<CacheImport>, Failure> {
    let mut out = Vec::new();
    if let Some(v) = opts.get("cache-imports").filter(|v| !v.is_empty()) {
        let list: Vec<serde_json::Value> = serde_json::from_str(v).map_err(|e| {
            Failure::new(format!(
                "failed to unmarshal cache-imports ({}): {e}",
                go_quote(v)
            ))
        })?;
        for entry in list {
            let kind = entry
                .get("Type")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            let attrs = entry
                .get("Attrs")
                .and_then(|a| a.as_object())
                .map(|a| {
                    a.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            out.push((kind, attrs));
        }
    }
    if let Some(v) = opts.get("cache-from").filter(|v| !v.is_empty()) {
        for r in v.split(',') {
            out.push((
                "registry".to_string(),
                BTreeMap::from([("ref".to_string(), r.to_string())]),
            ));
        }
    }
    Ok(out)
}

/// builder/caps.go's validateCaps: whether the build may go to another frontend, and the
/// refusal of a capability this one lacks.
fn validate_caps(req: Option<&String>) -> (bool, Option<Failure>) {
    let Some(req) = req.filter(|r| !r.is_empty()) else {
        return (false, None);
    };
    let mut forward = false;
    let mut refused = None;
    for c in req.split(',') {
        let name = c.split('+').next().unwrap_or_default();
        if !CAPS.contains(&name) {
            refused = Some(Failure {
                code: UNIMPLEMENTED,
                message: format!("unsupported frontend capability {name}"),
                details: vec![(
                    FRONTEND_CAP_DETAIL.to_string(),
                    format!("{{\"name\":{}}}", json_string(name)).into_bytes(),
                )],
            });
            if c.contains("+forward") {
                forward = true;
            } else {
                return (false, refused);
            }
        }
    }
    (forward, refused)
}

/// The file the build reads, as ReadEntrypoint loaded it: its name and content, and the
/// definition that loads it, which warnings and errors carry.
struct Entrypoint {
    filename: String,
    data: Vec<u8>,
    definition: Marshalled,
    definition_json: String,
}

impl Entrypoint {
    /// pb.SourceInfo of the file, as protobuf.
    fn info(&self) -> Vec<u8> {
        pb::source_info(&SourceInfo {
            filename: self.filename.as_bytes(),
            language: b"Dockerfile",
            data: &self.data,
            definition: Some(&self.definition.bytes),
        })
    }

    /// The head of its definition: the step warnings are shown on (`Definition.Head`).
    fn head(&self) -> String {
        self.definition
            .digests
            .last()
            .map(|d| String::from_utf8_lossy(d).into_owned())
            .unwrap_or_default()
    }

    /// `err` with the excerpts of `locations` (wrapSource, each location's
    /// errdefs.Source detail, the last wrapped outermost).
    fn wrap(&self, mut err: Failure, locations: &[Vec<(usize, usize)>]) -> Failure {
        let info = SourceInfo {
            filename: self.filename.as_bytes(),
            language: b"Dockerfile",
            data: &self.data,
            definition: None,
        };
        for loc in locations.iter().rev() {
            err.details.push((
                SOURCE_DETAIL.to_string(),
                pb::source_json(&info, &self.definition_json, loc).into_bytes(),
            ));
        }
        err
    }
}

/// A local source of one session, as dockerui loads a file from the client
/// (`llb.Local`): its follow paths, session, shared key hint, name and differ.
fn local_definition(name: &str, follow: &[&str], session: &str, key: &str, custom: &str) -> Definition {
    let mut attrs = BTreeMap::new();
    attrs.insert(b"local.differ".to_vec(), b"none".to_vec());
    let mut json = String::new();
    let follow: Vec<Vec<u8>> = follow.iter().map(|f| f.as_bytes().to_vec()).collect();
    shards_dockerfile::json::write_strings(&mut json, &follow);
    attrs.insert(b"local.followpaths".to_vec(), json.into_bytes());
    if !session.is_empty() {
        attrs.insert(b"local.session".to_vec(), session.as_bytes().to_vec());
    }
    attrs.insert(b"local.sharedkeyhint".to_vec(), key.as_bytes().to_vec());
    let mut meta = Meta::default();
    meta.description
        .insert(b"llb.customname".to_vec(), custom.as_bytes().to_vec());
    Definition {
        ops: vec![Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: format!("local://{name}").into_bytes(),
                attrs,
            },
            platform: None,
        }],
        metadata: vec![meta],
        root: Some(Input { op: 0, index: 0 }),
    }
}

/// What the frontend asks BuildKit, and keeps of its answers while it plans.
struct Gateway<'c, R, W> {
    client: RefCell<&'c mut Client<R, W>>,
    caps: Caps,
    env: Env,
    opts: BTreeMap<String, String>,
    /// A BuildKit call's error that ended the planning: the build's error then
    /// (grpcerrors keeps a status's message over the frontend's wrapping).
    failed: RefCell<Option<Failure>>,
    /// ResolveImageConfig's answers, by request (builder's withResolveCache).
    resolved: RefCell<BTreeMap<ImageRequest, Result<Resolved, Failure>>>,
    /// The Dockerfile's own .dockerignore, else the context's once read.
    dockerignore: RefCell<Option<Vec<u8>>>,
    /// The file being built, for warnings, while a plan for which they are sent is made.
    warn_into: RefCell<Option<(String, Vec<u8>)>>,
}

impl<R: Read, W: Write> Gateway<'_, R, W> {
    /// `def` solved (grpcClient.Solve): its capabilities checked against the worker's
    /// first, `cache-imports` and `cache-from` passed on as grpcclient passes them.
    fn solve(
        &self,
        def: &Marshalled,
        cache_imports: &[CacheImport],
        evaluate: bool,
    ) -> Result<Option<Ref>, Failure> {
        // In one order: the worker's map order chose which BuildKit's client names.
        for cap in &def.caps {
            if let Some(e) = self.caps.llb_refusal(cap, &self.env.product) {
                return Err(Failure::new(e));
            }
        }
        let mut frontend_opt = BTreeMap::new();
        for k in ["cache-imports", "cache-from"] {
            if let Some(v) = self.opts.get(k) {
                frontend_opt.insert(k.to_string(), v.clone());
            }
        }
        // Evaluated by BuildKit where it can, else by a stat of the result's root, as
        // grpcclient falls back.
        let native = evaluate && self.caps.has_frontend("gateway.solve.evaluate");
        let solved = self
            .client
            .borrow_mut()
            .solve(&Solve {
                definition: Some(&def.bytes),
                frontend_opt,
                cache_imports: cache_imports.to_vec(),
                evaluate: native,
                ..Solve::default()
            })
            .map_err(Failure::call)?;
        if evaluate
            && !native
            && let Some(r) = &solved.single
        {
            self.client
                .borrow_mut()
                .stat_file(&r.id, ".")
                .map_err(Failure::call)?;
        }
        Ok(solved.single)
    }

    /// dockerui's ReadFile: `name` of `r`, refused past 16 MiB by its size, read with a
    /// range one past it.
    fn read_file(&self, r: &Ref, name: &str) -> Result<Vec<u8>, ReadError> {
        let mut c = self.client.borrow_mut();
        if let Ok(st) = c.stat_file(&r.id, name)
            && st.size > MAX_FILE
        {
            return Err(ReadError::TooLarge(name.to_string()));
        }
        let data = c
            .read_file(&r.id, name, Some((0, MAX_FILE + 1)))
            .map_err(|e| ReadError::Call(Failure::call(e)))?;
        if data.len() as i64 > MAX_FILE {
            return Err(ReadError::TooLarge(name.to_string()));
        }
        Ok(data)
    }

    /// ResolveImageConfig of `name` for `platform`, its step named `log`, as grpcclient
    /// asks it through ResolveSourceMeta (its resolve mode not asked, as there).
    fn image_config(&self, name: &[u8], platform: &Platform, log: &[u8]) -> Result<Resolved, Failure> {
        let key = (name.to_vec(), platform::format_all(platform), log.to_vec());
        if let Some(r) = self.resolved.borrow().get(&key) {
            return r.clone();
        }
        let name_s = String::from_utf8_lossy(name).into_owned();
        let p = gateway_platform(platform);
        let answer = self
            .client
            .borrow_mut()
            .resolve_source_meta(
                &format!("docker-image://{name_s}"),
                &BTreeMap::new(),
                Some(&p),
                &String::from_utf8_lossy(log),
                "",
                false,
            )
            .map_err(Failure::call)
            .and_then(|meta| match meta.image {
                Some(image) => Ok(Resolved {
                    reference: meta
                        .identifier
                        .strip_prefix("docker-image://")
                        .unwrap_or(&meta.identifier)
                        .as_bytes()
                        .to_vec(),
                    digest: (!image.digest.is_empty()).then(|| image.digest.into_bytes()),
                    config: image.config,
                }),
                // imageutil.ResolveToNonImageError: a policy made it another source.
                None => Err(Failure::new(format!(
                    "ref {name_s} was resolved to non-image {}",
                    meta.identifier
                ))),
            });
        self.resolved.borrow_mut().insert(key, answer.clone());
        answer
    }
}

/// A platform as ops.proto's Platform has it.
fn gateway_platform(platform: &Platform) -> gw::Platform {
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    gw::Platform {
        architecture: s(&platform.architecture),
        os: s(&platform.os),
        variant: s(&platform.variant),
        os_version: s(&platform.os_version),
        os_features: platform.os_features.iter().map(|f| s(f)).collect(),
    }
}

/// Why a file could not be read: too large, or a call's error.
enum ReadError {
    TooLarge(String),
    Call(Failure),
}

impl ReadError {
    fn failure(self) -> Failure {
        match self {
            ReadError::TooLarge(name) => Failure::new(format!(
                "{name} exceeds maximum allowed size of {} bytes",
                MAX_FILE
            )),
            ReadError::Call(f) => f,
        }
    }
}

impl<R: Read, W: Write> Resolver for Gateway<'_, R, W> {
    fn resolve(&self, name: &[u8], platform: &Platform, log: &[u8]) -> Result<Resolved, Vec<u8>> {
        self.image_config(name, platform, log).map_err(|f| {
            let message = f.message.clone().into_bytes();
            *self.failed.borrow_mut() = Some(f);
            message
        })
    }

    fn epoch(&self, source: &EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>> {
        let _ = source;
        Err(b"shards' frontend takes SOURCE_DATE_EPOCH as a number of seconds only yet".to_vec())
    }

    fn context_excludes(&self) -> Result<Option<Vec<Vec<u8>>>, Vec<u8>> {
        let known = self.dockerignore.borrow().clone();
        let text = match known {
            Some(t) => t,
            None => {
                // DockerIgnorePatterns: the context's .dockerignore, read alone.
                let session = self
                    .opts
                    .get("local-sessionid:context")
                    .cloned()
                    .unwrap_or_else(|| self.env.session.clone());
                let def = local_definition(
                    "context",
                    &[".dockerignore"],
                    &session,
                    "context-.dockerignore",
                    "[internal] load .dockerignore",
                );
                let m = pb::definition(&def, &Carried::default()).ok_or_else(|| b"marshal".to_vec())?;
                let r = self.solve(&m, &[], false).map_err(|f| {
                    let message = f.message.clone().into_bytes();
                    *self.failed.borrow_mut() = Some(f);
                    message
                })?;
                let text = match r {
                    Some(r) => match self.read_file(&r, ".dockerignore") {
                        Ok(t) => t,
                        // A missing file is no error, an oversized one is.
                        Err(ReadError::TooLarge(n)) => {
                            let f = ReadError::TooLarge(n).failure();
                            let message = f.message.clone().into_bytes();
                            *self.failed.borrow_mut() = Some(f);
                            return Err(message);
                        }
                        Err(ReadError::Call(_)) => Vec::new(),
                    },
                    None => Vec::new(),
                };
                *self.dockerignore.borrow_mut() = Some(text.clone());
                text
            }
        };
        Ok(Some(shards_dockerfile::ignore::read_all(&text)))
    }

    fn local_excludes(&self, key: &[u8], name: &[u8]) -> Result<Option<Vec<Vec<u8>>>, Vec<u8>> {
        // NamedContext.Load of `local:NAME`: its .dockerignore alone, evaluated.
        let (key, name) = (String::from_utf8_lossy(key), String::from_utf8_lossy(name));
        let session = self
            .opts
            .get(&format!("local-sessionid:{name}"))
            .cloned()
            .unwrap_or_else(|| self.env.session.clone());
        let def = local_definition(
            &name,
            &[".dockerignore"],
            &session,
            &format!("context:{key}-.dockerignore"),
            &format!("[context {key}] load .dockerignore"),
        );
        let failed = |f: Failure| {
            let message = f.message.clone().into_bytes();
            *self.failed.borrow_mut() = Some(f);
            message
        };
        let m = pb::definition(&def, &Carried::default()).ok_or_else(|| b"marshal".to_vec())?;
        let Some(r) = self.solve(&m, &[], true).map_err(failed)? else {
            return Ok(Some(Vec::new()));
        };
        let text = match self.read_file(&r, ".dockerignore") {
            Ok(t) => t,
            Err(ReadError::TooLarge(n)) => return Err(failed(ReadError::TooLarge(n).failure())),
            Err(ReadError::Call(_)) => Vec::new(),
        };
        Ok(Some(shards_dockerfile::ignore::read_all(&text)))
    }

    fn resolve_layout(
        &self,
        name: &[u8],
        store: &[u8],
        platform: &Platform,
        log: &[u8],
    ) -> Result<Resolved, Vec<u8>> {
        // ResolveImageConfig with an OCI layout store, asked through ResolveSourceMeta:
        // the client's session serves the store.
        let attrs = BTreeMap::from([
            ("oci.session".to_string(), self.env.session.clone()),
            (
                "oci.store".to_string(),
                String::from_utf8_lossy(store).into_owned(),
            ),
        ]);
        let p = gateway_platform(platform);
        let answer = self
            .client
            .borrow_mut()
            .resolve_source_meta(
                &format!("oci-layout://{}", String::from_utf8_lossy(name)),
                &attrs,
                Some(&p),
                &String::from_utf8_lossy(log),
                "",
                false,
            )
            .map_err(Failure::call)
            .and_then(|meta| match meta.image {
                Some(image) => Ok(Resolved {
                    reference: meta
                        .identifier
                        .strip_prefix("oci-layout://")
                        .unwrap_or(&meta.identifier)
                        .as_bytes()
                        .to_vec(),
                    digest: (!image.digest.is_empty()).then(|| image.digest.into_bytes()),
                    config: image.config,
                }),
                None => Err(Failure::new(format!(
                    "ref {} was resolved to non-image {}",
                    String::from_utf8_lossy(name),
                    meta.identifier
                ))),
            });
        answer.map_err(|f| {
            let message = f.message.clone().into_bytes();
            *self.failed.borrow_mut() = Some(f);
            message
        })
    }

    fn warn(&self, w: &Warning) {
        let target = self.warn_into.borrow().clone();
        let Some((head, info)) = target else {
            return;
        };
        let start = w.location.first().map_or(0, |l| l.0);
        let mut short = format!("{}: {}", w.rule, String::from_utf8_lossy(&w.message));
        if start > 0 {
            short.push_str(&format!(" (line {start})"));
        }
        let ranges: Vec<Vec<u8>> = w.location.iter().map(|&(a, b)| pb::range(a, b)).collect();
        // A warning BuildKit does not take changes nothing of the build.
        let _ = self.client.borrow_mut().warn(&gw::Warning {
            digest: &head,
            level: 1,
            short: short.as_bytes(),
            detail: &[w.description.as_bytes().to_vec()],
            url: w.url,
            info: Some(&info),
            ranges: &ranges,
        });
    }
}

/// What the frontend returns.
enum Outcome {
    Built(Returned),
    /// Another frontend's result, which the file named.
    Forwarded(Vec<u8>),
}

/// `shards frontend`, which BuildKit runs.
pub(crate) fn frontend(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<OsString> = args.collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        let _ = writeln!(
            std::io::stdout(),
            "Usage:  shards frontend\n\nRun as BuildKit's frontend: the gateway on stdin and stdout, as a `# syntax=` line\nnaming shards' image has BuildKit run it."
        );
        return ExitCode::SUCCESS;
    }
    if let Some(a) = args.first() {
        // Nothing to run with: BuildKit runs the image's entrypoint as it is.
        let _ = writeln!(
            std::io::stderr(),
            "shards frontend: takes no arguments, got {:?}",
            a.to_string_lossy()
        );
        return ExitCode::from(2);
    }
    let Some(env) = Env::read(std::env::vars_os()) else {
        let _ = writeln!(
            std::io::stderr(),
            "shards frontend: BuildKit runs this, its gateway on stdin and stdout; there is no BUILDKIT_SESSION_ID here"
        );
        return ExitCode::from(2);
    };
    let (input, output) = match stdio() {
        Ok(io) => io,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards frontend: {e}");
            return ExitCode::FAILURE;
        }
    };
    match serve_on(input, output, &env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards frontend: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The gateway's connection: this process's stdin and stdout, moved to descriptors of
/// their own, stdout pointed at stderr from here, so nothing else the process writes can
/// reach the connection.
#[cfg(unix)]
fn stdio() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::AsFd as _;
    let input = std::io::stdin().as_fd().try_clone_to_owned()?;
    let output = std::io::stdout().as_fd().try_clone_to_owned()?;
    // SAFETY: dup2 on descriptors this process holds; it changes only what fd 1 names.
    if unsafe { libc::dup2(2, 1) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((std::fs::File::from(input), std::fs::File::from(output)))
}

#[cfg(not(unix))]
fn stdio() -> std::io::Result<(std::io::Stdin, std::io::Stdout)> {
    Ok((std::io::stdin(), std::io::stdout()))
}

/// The build on the gateway at `input` and `output`, its result or error returned.
fn serve_on<R: Read, W: Write>(input: R, output: W, env: &Env) -> Result<(), grpc::Error> {
    let mut client = Client::new(input, output)?;
    let outcome = build(&mut client, env);
    match outcome {
        Ok(Outcome::Built(r)) => client.return_result(Ok(&r)),
        Ok(Outcome::Forwarded(raw)) => client.return_raw(&raw),
        Err(f) => client.return_result(Err(&f.status())),
    }
}

/// builder.Build.
fn build<R: Read, W: Write>(client: &mut Client<R, W>, env: &Env) -> Result<Outcome, Failure> {
    let pong = client.ping().map_err(Failure::call)?;
    let caps = Caps::of(&pong);
    let mut opts = env.opts.clone();
    let config = config(&mut opts, env, &caps)?;
    let (allow_forward, caps_error) = validate_caps(opts.get("frontend.caps"));
    if !allow_forward && let Some(e) = caps_error.clone() {
        return Err(e);
    }
    let gateway = Gateway {
        client: RefCell::new(client),
        caps,
        env: env.clone(),
        opts: opts.clone(),
        failed: RefCell::new(None),
        resolved: RefCell::new(BTreeMap::new()),
        dockerignore: RefCell::new(None),
        warn_into: RefCell::new(None),
    };
    let mut config = config;
    let entry = read_entrypoint(&gateway, &mut config)?;
    if !opts.contains_key("cmdline") {
        if let Some(cmdline) = opts.get("build-arg:BUILDKIT_SYNTAX") {
            let first = cmdline.split_whitespace().next().unwrap_or_default();
            return forward(&gateway, first, cmdline);
        }
        if let Some((name, cmdline, line)) = shards_dockerfile::parser::detect_syntax(&entry.data) {
            let name = String::from_utf8_lossy(&name).into_owned();
            let cmdline = String::from_utf8_lossy(&cmdline).into_owned();
            return forward(&gateway, &name, &cmdline).map_err(|f| {
                if f.details.iter().any(|(url, _)| url == SOURCE_DETAIL) {
                    f
                } else {
                    entry.wrap(f, &[vec![(line, line)]])
                }
            });
        }
    }
    if let Some(e) = caps_error {
        return Err(e);
    }
    if let Some(req) = opts.get("requestid") {
        return subrequest(&gateway, &config, &entry, req);
    }
    build_platforms(&gateway, &config, &entry)
}

/// dockerui's HandleSubrequest, as builder.Build's handlers answer each: its result's
/// `result.json` (MarshalIndent), `result.txt`, `version` (and the lint's
/// `result.statuscode`), and no ref.
fn subrequest<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    config: &Config,
    entry: &Entrypoint,
    req: &str,
) -> Result<Outcome, Failure> {
    use shards_dockerfile::subrequests;
    let failed = |e: plan::Error| {
        let base = g
            .failed
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Failure::new(String::from_utf8_lossy(&e.message)));
        entry.wrap(base, &e.location)
    };
    let mut metadata: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    match req {
        "frontend.subrequests.describe" => {
            metadata.insert("result.json".into(), subrequests::DESCRIBE.as_bytes().to_vec());
            metadata.insert("result.txt".into(), subrequests::describe_text().into_bytes());
        }
        "frontend.outline" => {
            *g.failed.borrow_mut() = None;
            *g.warn_into.borrow_mut() = Some((entry.head(), entry.info()));
            let outline = plan::outline(&entry.data, &config.options, g);
            *g.warn_into.borrow_mut() = None;
            let o = outline.map_err(failed)?;
            metadata.insert("result.json".into(), o.json().into_bytes());
            metadata.insert("result.txt".into(), o.text().into_bytes());
        }
        "frontend.targets" => {
            let t = plan::targets(&entry.data, config.options.dialect).map_err(failed)?;
            metadata.insert("result.json".into(), t.json().into_bytes());
            metadata.insert("result.txt".into(), t.text().into_bytes());
        }
        "frontend.lint" => {
            *g.failed.borrow_mut() = None;
            let lint = plan::lint(&entry.data, &config.options, g).map_err(failed)?;
            let results = subrequests::LintResults {
                warnings: &lint.warnings,
                filename: entry.filename.as_bytes(),
                data: &entry.data,
                language: b"Dockerfile",
                definition: Some(&entry.definition_json),
                error: lint.error.as_ref().map(|(m, loc)| (m.as_slice(), loc)),
            };
            metadata.insert("result.json".into(), results.json().into_bytes());
            metadata.insert(
                "result.txt".into(),
                crate::build::lint_text(&lint.warnings, &entry.filename, &entry.data).into_bytes(),
            );
            let status = !lint.warnings.is_empty() || lint.error.is_some();
            metadata.insert(
                "result.statuscode".into(),
                if status { b"1".to_vec() } else { b"0".to_vec() },
            );
        }
        other => {
            return Err(Failure {
                code: UNKNOWN,
                message: format!("unsupported request {other}"),
                details: vec![(
                    SUBREQUEST_DETAIL.to_string(),
                    format!("{{\"name\":{}}}", json_string(other)).into_bytes(),
                )],
            });
        }
    }
    metadata.insert("version".into(), b"1.0.0".to_vec());
    Ok(Outcome::Built(Returned {
        single: Some(Ref::default()),
        refs: BTreeMap::new(),
        metadata,
    }))
}

/// ReadEntrypoint: the file the build reads, from the client's `dockerfile` directory
/// (initContext: the frontend's inputs asked for first, where BuildKit has them, none of
/// which this frontend reads yet).
fn read_entrypoint<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    config: &mut Config,
) -> Result<Entrypoint, Failure> {
    if let Some((def, filename)) = remote_context(g, config)? {
        return read_from(g, &def, &filename, None);
    }
    if g.caps.has_frontend("frontend.inputs") {
        let inputs = g.client.borrow_mut().inputs().map_err(Failure::call)?;
        let dockerfile =
            !config.force_local_dockerfile && inputs.contains_key(config.dockerfile_local.as_str());
        if dockerfile || inputs.contains_key("context") {
            return Err(Failure::new(
                "shards' frontend reads no frontend inputs yet: give the Dockerfile and the context from the client",
            ));
        }
    }
    let filename = config.filename.clone();
    let mut follow = vec![filename.clone(), format!("{filename}.dockerignore")];
    // dockerfile is also supported casing moby/moby#10858: path.Join(path.Dir(f), "dockerfile").
    let (dir, base) = match filename.rfind('/') {
        Some(i) => (
            filename.get(..i).unwrap_or_default(),
            filename.get(i + 1..).unwrap_or_default(),
        ),
        None => ("", filename.as_str()),
    };
    let lower =
        String::from_utf8_lossy(&shards_image::go::join(&[dir.as_bytes(), b"dockerfile"])).into_owned();
    if base == "Dockerfile" {
        follow.push(lower.clone());
    }
    let local = &config.dockerfile_local;
    let session = g
        .opts
        .get(&format!("local-sessionid:{local}"))
        .cloned()
        .unwrap_or_else(|| g.env.session.clone());
    let follow_refs: Vec<&str> = follow.iter().map(String::as_str).collect();
    let def = local_definition(
        local,
        &follow_refs,
        &session,
        local,
        &format!("[internal] load build definition from {filename}"),
    );
    let lower = (base == "Dockerfile").then_some(lower);
    read_from(g, &def, &filename, lower.as_deref())
}

/// The file `filename` of what `def` loads (Docker's other casing, `lower`, where the
/// first is not there), and its own `.dockerignore` beside it.
fn read_from<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    def: &Definition,
    filename: &str,
    lower: Option<&str>,
) -> Result<Entrypoint, Failure> {
    let marshalled = pb::definition(def, &Carried::default())
        .ok_or_else(|| Failure::new("failed to marshal local source"))?;
    let definition_json = pb::definition_json(def, &Carried::default())
        .ok_or_else(|| Failure::new("failed to marshal local source"))?;
    let r = g
        .solve(&marshalled, &[], false)?
        .ok_or_else(|| Failure::new("failed to resolve dockerfile: no result"))?;
    // "failed to read dockerfile: ", which ToGRPC keeps of no status of BuildKit's.
    let data = match g.read_file(&r, filename) {
        Ok(d) => d,
        Err(ReadError::TooLarge(n)) => {
            let f = ReadError::TooLarge(n).failure();
            return Err(Failure::new(format!("failed to read dockerfile: {}", f.message)));
        }
        Err(ReadError::Call(f)) => match lower {
            Some(lower) => match g.read_file(&r, lower) {
                Ok(d) => d,
                Err(_) => return Err(f),
            },
            None => return Err(f),
        },
    };
    let dockerignore = match g.read_file(&r, &format!("{filename}.dockerignore")) {
        Ok(d) => Some(d),
        Err(ReadError::TooLarge(n)) => return Err(ReadError::TooLarge(n).failure()),
        Err(ReadError::Call(_)) => None,
    };
    *g.dockerignore.borrow_mut() = dockerignore;
    Ok(Entrypoint {
        filename: filename.to_string(),
        data,
        definition: marshalled,
        definition_json,
    })
}

/// initContext's remote contexts: a Git repository (DetectGitContext), whose source the
/// file is read from too; an HTTP(S) download (DetectHTTPContext), solved and its first
/// 1024 bytes read, unpacked onto scratch where they are an archive's, else itself the
/// file, named `context`. The definition the file is read from, and the file's name; none
/// for the client's own directory.
fn remote_context<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    config: &mut Config,
) -> Result<Option<(Definition, String)>, Failure> {
    // The file comes from the remote context unscoped: contextsubdir scopes the context.
    let base = config.options.clone();
    let unscoped = |main: MainContext| Options {
        main_context: main,
        context_subdir: None,
        ..base.clone()
    };
    match config.options.main_context.clone() {
        MainContext::Local => Ok(None),
        main @ MainContext::Git { .. } => {
            let def = plan::context_definition(&unscoped(main))
                .map_err(|e| Failure::new(String::from_utf8_lossy(&e)))?;
            Ok(Some((def, config.filename.clone())))
        }
        MainContext::Http { url, .. } => {
            let first = MainContext::Http {
                url: url.clone(),
                archive: false,
            };
            let def = plan::context_definition(&unscoped(first.clone()))
                .map_err(|e| Failure::new(String::from_utf8_lossy(&e)))?;
            let m = pb::definition(&def, &Carried::default())
                .ok_or_else(|| Failure::new("failed to marshal httpcontext"))?;
            let r = g
                .solve(&m, &[], false)?
                .ok_or_else(|| Failure::new("failed to resolve httpcontext: no result"))?;
            let head = g
                .client
                .borrow_mut()
                .read_file(&r.id, "context", Some((0, 1024)))
                .map_err(Failure::call)?;
            let archive = shards_dockerfile::dockerui::is_archive(&head);
            config.options.main_context = MainContext::Http { url, archive };
            if archive {
                let def = plan::context_definition(&unscoped(config.options.main_context.clone()))
                    .map_err(|e| Failure::new(String::from_utf8_lossy(&e)))?;
                Ok(Some((def, config.filename.clone())))
            } else {
                config.filename = "context".to_string();
                Ok(Some((def, config.filename.clone())))
            }
        }
    }
}

/// forwardGateway: the build handed to the frontend the file names, its result returned
/// as it came.
fn forward<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    source: &str,
    cmdline: &str,
) -> Result<Outcome, Failure> {
    let mut opts = g.opts.clone();
    opts.insert("cmdline".into(), cmdline.to_string());
    opts.insert("source".into(), source.to_string());
    let mut inputs = BTreeMap::new();
    if g.caps.has_frontend("frontend.inputs") {
        inputs = g.client.borrow_mut().inputs().map_err(Failure::call)?;
    }
    let solved = g
        .client
        .borrow_mut()
        .solve(&Solve {
            frontend: "gateway.v0",
            frontend_opt: opts,
            frontend_inputs: inputs,
            ..Solve::default()
        })
        .map_err(Failure::call)?;
    Ok(Outcome::Forwarded(solved.result))
}

/// dockerui's Build over each target platform (or the implicit one), each planned,
/// solved, and returned with its image config; then Finalize's platforms.
fn build_platforms<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    config: &Config,
    entry: &Entrypoint,
) -> Result<Outcome, Failure> {
    let targets: Vec<Option<Platform>> = if config.target_platforms.is_empty() {
        vec![None]
    } else {
        config.target_platforms.iter().cloned().map(Some).collect()
    };
    let mut returned = Returned::default();
    let mut platforms_json = Vec::with_capacity(targets.len());
    for (i, target) in targets.iter().enumerate() {
        let mut options = config.options.clone();
        if let Some(t) = target {
            options.target_platform = t.clone();
            options.implicit_target = false;
        }
        // Warnings from the first platform's plan alone.
        *g.warn_into.borrow_mut() = (i == 0).then(|| (entry.head(), entry.info()));
        *g.failed.borrow_mut() = None;
        let planned = plan::plan(&entry.data, &options, g);
        *g.warn_into.borrow_mut() = None;
        let planned = match planned {
            Ok(p) => p,
            Err(e) => {
                let base = g
                    .failed
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| Failure::new(String::from_utf8_lossy(&e.message)));
                return Err(entry.wrap(base, &e.location));
            }
        };
        let carried = Carried {
            source: Some(SourceInfo {
                filename: entry.filename.as_bytes(),
                language: b"Dockerfile",
                data: &entry.data,
                definition: Some(&entry.definition.bytes),
            }),
            sets_default_path: g.caps.llb_refusal("exec.meta.setsdefaultpath", "").is_none(),
            group_prefix: &format!("{}-", g.env.session),
        };
        let def = pb::definition(&planned.definition(), &carried)
            .ok_or_else(|| Failure::new("failed to marshal LLB definition"))?;
        let r = g.solve(&def, &config.cache_imports, false)?.unwrap_or_default();
        let config_json = planned.image.to_json().map_err(|e| {
            Failure::new(format!(
                "failed to marshal image config: {}",
                String::from_utf8_lossy(&e)
            ))
        })?;
        let base_json = match &planned.base_image {
            Some(b) => Some(b.to_json().map_err(|e| {
                Failure::new(format!(
                    "failed to marshal source image config: {}",
                    String::from_utf8_lossy(&e)
                ))
            })?),
            None => None,
        };
        let p = platform::normalize(&target.clone().unwrap_or_else(default_spec));
        let id = String::from_utf8_lossy(&platform::format_all(&p)).into_owned();
        let key = |k: &str| {
            if config.multi_platform {
                format!("{k}/{id}")
            } else {
                k.to_string()
            }
        };
        returned
            .metadata
            .insert(key("containerimage.config"), config_json.into_bytes());
        if let Some(b) = base_json {
            returned
                .metadata
                .insert(key("containerimage.base.config"), b.into_bytes());
        }
        if let Some(e) = planned.epoch {
            returned
                .metadata
                .insert(key("source.date.epoch"), e.to_string().into_bytes());
        }
        if config.multi_platform {
            returned.refs.insert(id.clone(), r);
        } else {
            returned.single = Some(r);
        }
        platforms_json.push(format!(
            "{{\"ID\":{},\"Platform\":{}}}",
            json_string(&id),
            platform_json(&p)
        ));
    }
    returned.metadata.insert(
        "refs.platforms".into(),
        format!("{{\"Platforms\":[{}]}}", platforms_json.join(",")).into_bytes(),
    );
    Ok(Outcome::Built(returned))
}

fn json_string(s: &str) -> String {
    let mut out = String::new();
    shards_dockerfile::json::write_string(&mut out, s.as_bytes());
    out
}

/// An OCI platform as encoding/json writes ocispecs.Platform.
fn platform_json(p: &Platform) -> String {
    let s = |b: &[u8]| {
        let mut out = String::new();
        shards_dockerfile::json::write_string(&mut out, b);
        out
    };
    let mut fields = vec![
        format!("\"architecture\":{}", s(&p.architecture)),
        format!("\"os\":{}", s(&p.os)),
    ];
    if !p.os_version.is_empty() {
        fields.push(format!("\"os.version\":{}", s(&p.os_version)));
    }
    if !p.os_features.is_empty() {
        let mut list = String::new();
        shards_dockerfile::json::write_strings(&mut list, &p.os_features);
        fields.push(format!("\"os.features\":{list}"));
    }
    if !p.variant.is_empty() {
        fields.push(format!("\"variant\":{}", s(&p.variant)));
    }
    format!("{{{}}}", fields.join(","))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    /// BuildKit's half of docker/dockerfile:1's build in the capture (BuildKit v0.28.1,
    /// Docker 29.3.1, D113's spy in shards-dind), and docker/dockerfile's own half.
    const SERVER: &[u8] = include_bytes!("../../gateway/testdata/dockerfile-1.server.bin");
    const CLIENT: &[u8] = include_bytes!("../../gateway/testdata/dockerfile-1.client.bin");

    /// The environment BuildKit gave docker/dockerfile in the capture.
    fn captured_env() -> Env {
        let opts = [
            ("attest:provenance", "mode=min,inline-only=true"),
            ("no-cache", ""),
            ("image-resolve-mode", "local"),
            ("cmdline", "127.0.0.1:15113/shards-d113-spy:1"),
            ("source", "127.0.0.1:15113/shards-d113-spy:1"),
            ("filename", "Dockerfile"),
        ];
        let vars = opts
            .iter()
            .enumerate()
            .map(|(i, (k, v))| {
                (
                    OsString::from(format!("BUILDKIT_FRONTEND_OPT_{i}")),
                    OsString::from(format!("{k}={v}")),
                )
            })
            .chain([
                (
                    OsString::from("BUILDKIT_SESSION_ID"),
                    OsString::from("i7nf753w4qqfpv48b2v4war4t"),
                ),
                (
                    OsString::from("BUILDKIT_WORKERS"),
                    OsString::from(
                        r#"[{"id":"iom3e7x2hniunl47jzmet6eqt","labels":{},"platforms":[{"architecture":"arm64","os":"linux"},{"architecture":"amd64","os":"linux"},{"architecture":"amd64","os":"linux","variant":"v2"}]}]"#,
                    ),
                ),
                (OsString::from("BUILDKIT_EXPORTEDPRODUCT"), OsString::new()),
            ]);
        Env::read(vars).unwrap()
    }

    /// Each stream's gRPC messages, by stream, from one side of an HTTP/2 connection.
    fn messages(mut b: &[u8]) -> BTreeMap<u32, Vec<Vec<u8>>> {
        const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        if b.starts_with(PREFACE) {
            b = &b[PREFACE.len()..];
        }
        let mut data: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        while b.len() >= 9 {
            let n = usize::from(b[0]) << 16 | usize::from(b[1]) << 8 | usize::from(b[2]);
            let (kind, flags) = (b[3], b[4]);
            let stream = u32::from_be_bytes([b[5] & 0x7f, b[6], b[7], b[8]]);
            let mut payload = &b[9..9 + n];
            if kind == 0 {
                if flags & 0x8 != 0 {
                    let pad = usize::from(payload[0]);
                    payload = &payload[1..payload.len() - pad];
                }
                data.entry(stream).or_default().extend_from_slice(payload);
            }
            b = &b[9 + n..];
        }
        data.into_iter()
            .map(|(s, d)| {
                let mut out = Vec::new();
                let mut at = 0;
                while at + 5 <= d.len() {
                    let n = u32::from_be_bytes([d[at + 1], d[at + 2], d[at + 3], d[at + 4]]) as usize;
                    out.push(d[at + 5..at + 5 + n].to_vec());
                    at += 5 + n;
                }
                (s, out)
            })
            .collect()
    }

    fn fields(b: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        let varint = |b: &[u8], at: &mut usize| {
            let (v, n) = shards_gateway::wire::varint(&b[*at..]).unwrap();
            *at += n;
            v
        };
        while at < b.len() {
            let tag = varint(b, &mut at);
            let value = match tag & 7 {
                0 => {
                    let start = at;
                    varint(b, &mut at);
                    b[start..at].to_vec()
                }
                2 => {
                    let n = varint(b, &mut at) as usize;
                    at += n;
                    b[at - n..at].to_vec()
                }
                w => panic!("wire type {w}"),
            };
            out.push((tag, value));
        }
        out
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Solve,
        Return,
        Result,
        Ref,
        Definition,
        OpMetadata,
        Source,
        SourceInfo,
        Opaque,
    }

    fn flat(fields: Vec<(u64, Vec<u8>)>) -> Vec<u8> {
        let mut out = Vec::new();
        for (tag, v) in fields {
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&(v.len() as u64).to_be_bytes());
            out.extend_from_slice(&v);
        }
        out
    }

    /// A message with each map's entries in one order, what it holds read the same way:
    /// BuildKit's client writes Go's map order.
    fn canonical(b: &[u8], kind: Kind) -> Vec<(u64, Vec<u8>)> {
        let mut plain = Vec::new();
        let mut maps = Vec::new();
        let entry = |v: &[u8], value: Kind| -> Vec<u8> {
            flat(
                fields(v)
                    .into_iter()
                    .map(|(t, x)| match t >> 3 {
                        2 => (t, flat(canonical(&x, value))),
                        _ => (t, x),
                    })
                    .collect(),
            )
        };
        for (tag, v) in fields(b) {
            match (kind, tag >> 3) {
                (Kind::Solve, 1) => plain.push((tag, flat(canonical(&v, Kind::Definition)))),
                (Kind::Solve, 3) => maps.push((tag, v)),
                (Kind::Return, 1) => plain.push((tag, flat(canonical(&v, Kind::Result)))),
                (Kind::Result, 10) => maps.push((tag, v)),
                // The definition BuildKit echoed, which Go's client writes again in its
                // own map order.
                (Kind::Result, 3) => plain.push((tag, flat(canonical(&v, Kind::Ref)))),
                (Kind::Ref, 2) => plain.push((tag, flat(canonical(&v, Kind::Definition)))),
                (Kind::Definition, 2) => maps.push((tag, entry(&v, Kind::OpMetadata))),
                (Kind::Definition, 3) => plain.push((tag, flat(canonical(&v, Kind::Source)))),
                (Kind::OpMetadata, 2 | 5) | (Kind::Source, 1) => maps.push((tag, v)),
                (Kind::Source, 2) => plain.push((tag, flat(canonical(&v, Kind::SourceInfo)))),
                (Kind::SourceInfo, 3) => plain.push((tag, flat(canonical(&v, Kind::Definition)))),
                _ => plain.push((tag, v)),
            }
        }
        maps.sort();
        plain.extend(maps);
        plain
    }

    /// The image's label lists what the frontend can do, so that BuildKit refuses a build
    /// asking more before it runs it.
    #[test]
    fn the_images_caps_label_is_what_the_frontend_does() {
        let dockerfile = include_str!("../../../scripts/frontend/Dockerfile");
        let label = format!("LABEL moby.buildkit.frontend.caps=\"{}\"", CAPS.join(","));
        assert!(dockerfile.lines().any(|l| l == label), "{label}");
        assert!(
            dockerfile
                .lines()
                .any(|l| l == "LABEL moby.buildkit.frontend.network.none=\"true\"")
        );
        assert!(
            dockerfile
                .lines()
                .any(|l| l == "ENTRYPOINT [\"/shards\", \"frontend\"]")
        );
    }

    /// The build of the capture, its BuildKit answering as it answered docker/dockerfile:1:
    /// every call made in that frontend's order, each request what it asked (maps aside),
    /// the result returned with the same image config, base config and platforms.
    #[test]
    fn a_build_asks_and_returns_what_docker_dockerfile_did() {
        let mut written = Vec::new();
        serve_on(std::io::Cursor::new(SERVER), &mut written, &captured_env()).unwrap();
        let ours = messages(&written);
        let theirs = messages(CLIENT);
        assert_eq!(ours.keys().collect::<Vec<_>>(), theirs.keys().collect::<Vec<_>>());
        // Streams in call order: Ping, Inputs, Solve, StatFile, ReadFile, StatFile,
        // ReadFile, ResolveSourceMeta, Solve, StatFile, ReadFile, Solve, Return.
        let kinds = [
            Kind::Opaque,
            Kind::Opaque,
            Kind::Solve,
            Kind::Opaque,
            Kind::Opaque,
            Kind::Opaque,
            Kind::Opaque,
            Kind::Opaque,
            Kind::Solve,
            Kind::Opaque,
            Kind::Opaque,
            Kind::Solve,
            Kind::Return,
        ];
        assert_eq!(ours.len(), kinds.len());
        for ((stream, mine), kind) in ours.iter().zip(kinds) {
            let want = &theirs[stream];
            assert_eq!(mine.len(), want.len(), "stream {stream}");
            for (m, w) in mine.iter().zip(want) {
                assert_eq!(
                    canonical(m, kind),
                    canonical(w, kind),
                    "stream {stream}: ours {m:02x?}"
                );
            }
        }
    }
}
