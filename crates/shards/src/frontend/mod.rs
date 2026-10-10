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

use crate::build::skills;

mod check;
mod osi;

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
    /// The platform the frontend runs on, containerd's DefaultSpec, which dockerui takes
    /// where it is given none (the build's without workers, the result's without a target
    /// platform): this process's ([`default_spec`]).
    own: Platform,
}

impl Env {
    /// The environment's, or none where BuildKit gave no session: no gateway is there.
    fn read(vars: impl Iterator<Item = (OsString, OsString)>) -> Option<Env> {
        let mut env = Env {
            own: default_spec(),
            ..Env::default()
        };
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
        .unwrap_or_else(|| platform::normalize(&env.own));
    let mut target_platforms = Vec::new();
    if let Some(v) = opts.get("platform").filter(|v| !v.is_empty()) {
        for p in v.split(',') {
            let parsed = platform::parse(p.as_bytes(), &env.own).map_err(|e| {
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
/// The ops that lay an OSI artifact's content out (osi.rs), in a definition's order, the
/// last the step whose first output is it.
type Laid = Vec<(Op, Meta)>;

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
    /// The OSI artifacts build contexts gave, by the source the planner names each by
    /// (`osi-artifact://` and its reference with its manifest's digest): the ops that lay
    /// its content out, the last the step whose first output is it (osi.rs).
    artifacts: RefCell<BTreeMap<Vec<u8>, Laid>>,
    /// The frontend's own image, as a source op of the definition BuildKit mounts for it:
    /// the root of the steps it has BuildKit run (check.rs, osi.rs).
    me: Option<Op>,
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

    /// One blob of the OCI layout the client serves as content store `store`, by `digest`
    /// (BuildKit's `oci-layout+blob` source, which `llb.OCILayoutBlob` makes), as a file
    /// named `blob`.
    fn layout_blob(&self, store: &[u8], digest: &str) -> Result<Op, Failure> {
        let store_s = String::from_utf8_lossy(store);
        let at = shards_image::reference::Reference::parse_normalized(&format!("{store_s}@{digest}"))
            .map_err(|e| Failure::new(format!("{store_s}@{digest}: {e}")))?;
        Ok(Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: format!("oci-layout+blob://{at}").into_bytes(),
                attrs: BTreeMap::from([
                    (b"http.filename".to_vec(), b"blob".to_vec()),
                    (b"oci.session".to_vec(), self.env.session.clone().into_bytes()),
                    (b"oci.store".to_vec(), store.to_vec()),
                ]),
            },
            platform: None,
        })
    }

    /// A blob of a layout, read whole where it is at most `most` bytes: a manifest's, an
    /// index's or a config's.
    fn read_layout_blob(&self, store: &[u8], digest: &str, most: u64) -> Result<Vec<u8>, Failure> {
        let def = Definition {
            ops: vec![self.layout_blob(store, digest)?],
            metadata: vec![Meta::default()],
            root: Some(Input { op: 0, index: 0 }),
        };
        let m = pb::definition(&def, &Carried::default())
            .ok_or_else(|| Failure::new("failed to marshal LLB definition"))?;
        let r = self.solve(&m, &[], true)?.unwrap_or_default();
        let bytes = self.read_file(&r, "blob").map_err(ReadError::failure)?;
        if bytes.len() as u64 > most {
            return Err(Failure::new(format!("{digest}: over {most} bytes")));
        }
        Ok(bytes)
    }

    /// An OSI artifact of `kind` from the OCI layout the client serves as `store`, at
    /// `digest` (D113): held as `shards build` holds one from a registry (agent.rs, the
    /// same checks and words: D54, §12.17, §9.2), its content checked and laid out by a
    /// step of the frontend's own image, which the definition takes its place for.
    fn artifact_from_layout(
        &self,
        name: &[u8],
        kind: &[u8],
        store: &[u8],
        digest: &[u8],
    ) -> Result<Resolved, Failure> {
        use shards_image::oci::{self, Document};
        let name_s = String::from_utf8_lossy(name).into_owned();
        let want = match kind {
            b"agent" => shards_image::osi::Kind::Agent,
            b"harness" => shards_image::osi::Kind::Harness,
            _ => shards_image::osi::Kind::Mcp,
        };
        let media = |bytes: &[u8]| {
            serde_json::from_slice::<serde_json::Value>(bytes)
                .ok()
                .and_then(|v| v.get("mediaType").and_then(|m| m.as_str()).map(str::to_string))
                .unwrap_or_else(|| oci::media::OCI_MANIFEST.to_string())
        };
        // The root the context names: an index's manifest for this platform, as
        // `shards build` takes one (agent::fetch), else the manifest itself.
        let mut d = String::from_utf8_lossy(digest).into_owned();
        let mut bytes = self.read_layout_blob(store, &d, oci::MAX_MANIFEST)?;
        // Each document is the one its digest names, as a fetch into the store holds one.
        let held = |bytes: &[u8], d: &str| -> Result<(), Failure> {
            let got = osi::sha256_of(bytes).map_err(|e| Failure::new(format!("{d}: {e}")))?;
            if got != d {
                return Err(Failure::new(format!("got digest {got}, expected {d}")));
            }
            Ok(())
        };
        held(&bytes, &d)?;
        let document =
            |bytes: &[u8]| oci::parse_document(bytes, &media(bytes)).map_err(|e| Failure::new(e.to_string()));
        if let Document::Index(index) = document(&bytes)? {
            let chosen = shards_image::platform::select(&index, &shards_image::platform::guest())
                .cloned()
                .ok_or_else(|| Failure::new(format!("{name_s}: no {} for this platform", want.word())))?;
            d = chosen.digest.clone();
            bytes = self.read_layout_blob(store, &d, oci::MAX_MANIFEST)?;
            held(&bytes, &d)?;
        }
        let Document::Manifest(m) = document(&bytes)? else {
            return Err(Failure::new(format!("{name_s}: an index inside an index")));
        };
        let found = crate::agent::check_manifest(&d, &m).map_err(Failure::new)?;
        if found != want {
            return Err(Failure::new(crate::agent::not_the_kind(&name_s, found, want)));
        }
        let config = self.read_layout_blob(store, &m.config.digest, oci::MAX_CONFIG)?;
        held(&config, &m.config.digest)?;
        shards_image::osi::Config::parse(&config).map_err(|e| Failure::new(format!("{name_s}: {e}")))?;
        // The content: each layer from the layout, held and laid out in the frontend's own
        // image, with no network (osi.rs).
        let me = self.me.clone().ok_or_else(|| {
            Failure::new(
                "shards' frontend lays out an OSI artifact in its own image, which BuildKit gave no definition of (/run/config/buildkit/metadata/frontend.bin)",
            )
        })?;
        let spec = osi::Spec {
            kind: want,
            layers: m
                .layers
                .iter()
                .map(|l| (l.digest.clone(), l.media_type.clone()))
                .collect(),
        };
        let mut ops: Vec<(Op, Meta)> = vec![(me.clone(), Meta::default())];
        let mut mounts = vec![shards_dockerfile::llb::OpMount {
            input: 0,
            selector: Vec::new(),
            dest: b"/".to_vec(),
            output: -1,
            readonly: true,
            kind: shards_dockerfile::llb::OpMountKind::Bind,
        }];
        let mut inputs = vec![Input { op: 0, index: 0 }];
        for (i, (layer, _)) in spec.layers.iter().enumerate() {
            ops.push((self.layout_blob(store, layer)?, Meta::default()));
            inputs.push(Input {
                op: ops.len() - 1,
                index: 0,
            });
            mounts.push(shards_dockerfile::llb::OpMount {
                input: inputs.len() as i64 - 1,
                selector: Vec::new(),
                dest: format!("/layers/{i}").into_bytes(),
                output: -1,
                readonly: true,
                kind: shards_dockerfile::llb::OpMountKind::Bind,
            });
        }
        for (dest, output) in [(&b"/out"[..], 0), (&b"/report"[..], 1)] {
            mounts.push(shards_dockerfile::llb::OpMount {
                input: -1,
                selector: Vec::new(),
                dest: dest.to_vec(),
                output,
                readonly: false,
                kind: shards_dockerfile::llb::OpMountKind::Bind,
            });
        }
        let exec = Op {
            inputs,
            kind: OpKind::Exec {
                process: Box::new(shards_dockerfile::llb::Process {
                    args: vec![
                        b"/shards".to_vec(),
                        b"frontend".to_vec(),
                        b"osi".to_vec(),
                        spec.json().into_bytes(),
                    ],
                    cwd: b"/".to_vec(),
                    ..Default::default()
                }),
                mounts,
                network: shards_dockerfile::llb::NetMode::None,
                security: shards_dockerfile::llb::Security::Sandbox,
                secret_env: Vec::new(),
                devices: Vec::new(),
            },
            platform: me.platform.clone(),
        };
        let mut meta = Meta::default();
        meta.description.insert(
            b"llb.customname".to_vec(),
            format!("[internal] load {} {name_s}", want.word()).into_bytes(),
        );
        ops.push((exec, meta));
        // Its report solved and read before the build goes on: a refusal fails it here.
        let (o, md): (Vec<Op>, Vec<Meta>) = ops.iter().cloned().unzip();
        let def = Definition {
            ops: o,
            metadata: md,
            root: Some(Input {
                op: ops.len() - 1,
                index: 1,
            }),
        };
        let marshalled = pb::definition(&def, &Carried::default())
            .ok_or_else(|| Failure::new("failed to marshal LLB definition"))?;
        let r = self.solve(&marshalled, &[], true)?.unwrap_or_default();
        let finding = self.read_file(&r, "finding").map_err(ReadError::failure)?;
        if !finding.is_empty() {
            return Err(Failure::new(format!(
                "{name_s}: {}",
                String::from_utf8_lossy(&finding)
            )));
        }
        let mut reference = shards_image::reference::Reference::parse(&name_s)
            .map_err(|e| Failure::new(format!("{name_s}: {e}")))?;
        reference.digest =
            Some(shards_image::reference::Digest::parse(&d).map_err(|e| Failure::new(format!("{d}: {e}")))?);
        let resolved = reference.to_string();
        self.artifacts
            .borrow_mut()
            .insert([b"osi-artifact://".as_slice(), resolved.as_bytes()].concat(), ops);
        Ok(Resolved {
            reference: resolved.into_bytes(),
            digest: Some(d.into_bytes()),
            config,
        })
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

    /// An OSI artifact named by its reference alone: refused, saying what to give. BuildKit
    /// gives a frontend no artifact's manifest (D113, measured), so one comes from an OCI
    /// layout the client gives as a named build context, keyed as a FROM's is.
    fn artifact(&self, name: &[u8], kind: &[u8], log: &[u8]) -> Result<Resolved, Vec<u8>> {
        let _ = (kind, log);
        let name = String::from_utf8_lossy(name);
        let key = shards_image::reference::Reference::parse_normalized(&name).map_or_else(
            |_| name.to_string(),
            |r| {
                let f = r.familiar();
                f.strip_suffix(":latest").unwrap_or(&f).to_string()
            },
        );
        Err(format!(
            "{name}: through BuildKit an OSI artifact comes from an OCI layout given as a build context, BuildKit giving a frontend no artifact's manifest: --build-context {key}=oci-layout://DIR:TAG, which `oras cp --to-oci-layout {name} DIR:TAG` or `skopeo copy --preserve-digests docker://{name} oci:DIR:TAG` makes"
        )
        .into_bytes())
    }

    fn artifact_layout(
        &self,
        name: &[u8],
        kind: &[u8],
        store: &[u8],
        digest: &[u8],
        log: &[u8],
    ) -> Result<Resolved, Vec<u8>> {
        let _ = log;
        self.artifact_from_layout(name, kind, store, digest).map_err(|f| {
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
    // The isolation checks the frontend has BuildKit run in its own image (check.rs), and
    // the layout of an OSI artifact a build context gives (osi.rs).
    if let [mode, spec] = args.as_slice() {
        if mode == "check" {
            return check::run(&spec.to_string_lossy());
        }
        if mode == "osi" {
            return osi::run(&spec.to_string_lossy());
        }
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
        artifacts: RefCell::new(BTreeMap::new()),
        me: self_image(),
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
        let mut plan_def = planned.definition();
        lay_skills(g, &mut plan_def, &carried, entry)?;
        lay_artifacts(g, &mut plan_def)?;
        if !planned.domains.is_empty() {
            check_domains(g, g.me.clone(), &plan_def, &planned.domains, &carried, entry)?;
        }
        // The guards' marks are shards' own, for the checks just run: BuildKit is given
        // none of them.
        for md in &mut plan_def.metadata {
            md.description.remove(plan::GUARD);
            md.description.remove(plan::OWN);
        }
        let def = pb::definition(&plan_def, &carried)
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
        let p = platform::normalize(&target.clone().unwrap_or_else(|| g.env.own.clone()));
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
        // An Agentfile's manifest annotations (D57), as shards build writes them, given to
        // BuildKit's image exporter as the result's `annotation-manifest` metadata.
        for (k, v) in agentfile_annotations(&planned) {
            let name = if config.multi_platform {
                format!("annotation-manifest[{id}].{k}")
            } else {
                format!("annotation-manifest.{k}")
            };
            returned.metadata.insert(name, v);
        }
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

/// An Agentfile's image's manifest annotations (D57): the normalized Agentfile's digest,
/// its config label's, and its agents' and harnesses' names, each list comma-separated in
/// name order, as shards build annotates its manifest.
fn agentfile_annotations(planned: &plan::Plan) -> Vec<(String, Vec<u8>)> {
    use shards_dockerfile::agentfile;
    let mut out = Vec::new();
    let Some(digest) = planned.image.config.labels.get(agentfile::DIGEST_LABEL) else {
        return out;
    };
    out.push((
        String::from_utf8_lossy(agentfile::DIGEST_LABEL).into_owned(),
        digest.clone(),
    ));
    for (key, harness) in [
        (agentfile::AGENTS_ANNOTATION, false),
        (agentfile::HARNESSES_ANNOTATION, true),
    ] {
        let mut names: Vec<&[u8]> = planned
            .domains
            .iter()
            .filter(|d| d.harness == harness)
            .map(|d| d.name.as_slice())
            .collect();
        names.sort_unstable();
        names.dedup();
        if !names.is_empty() {
            out.push((String::from_utf8_lossy(key).into_owned(), names.join(&b","[..])));
        }
    }
    out
}

/// The ops `def` needs for `input`, in its order, numbered anew, and `input` among them.
fn closure(def: &Definition, input: Input) -> Option<Definition> {
    let mut needed = vec![false; def.ops.len()];
    let mut stack = vec![input.op];
    while let Some(at) = stack.pop() {
        let slot = needed.get_mut(at)?;
        if *slot {
            continue;
        }
        *slot = true;
        stack.extend(def.ops.get(at)?.inputs.iter().map(|i| i.op));
    }
    let mut renumbered = vec![usize::MAX; def.ops.len()];
    let mut out = Definition::default();
    for (i, (op, meta)) in def.ops.iter().zip(&def.metadata).enumerate() {
        if !needed.get(i).copied().unwrap_or(false) {
            continue;
        }
        let mut op = op.clone();
        for inp in &mut op.inputs {
            inp.op = *renumbered.get(inp.op)?;
        }
        if let Some(slot) = renumbered.get_mut(i) {
            *slot = out.ops.len();
        }
        out.ops.push(op);
        out.metadata.push(meta.clone());
    }
    out.root = Some(Input {
        op: *renumbered.get(input.op)?,
        index: input.index,
    });
    Some(out)
}

/// The frontend's own image as BuildKit gave its definition (gateway.go's metadataMount,
/// /run/config/buildkit/metadata/frontend.bin): its source op, that the isolation checks
/// run in it.
fn self_image() -> Option<Op> {
    let bytes = std::fs::read("/run/config/buildkit/metadata/frontend.bin").ok()?;
    source_op_of(&bytes)
}

/// The first source op of a pb.Definition: its identifier, attributes and platform.
fn source_op_of(def: &[u8]) -> Option<Op> {
    use shards_gateway::wire::Reader;
    let text = |v: shards_gateway::wire::Value<'_>| v.bytes().ok().map(<[u8]>::to_vec);
    for f in Reader::new(def) {
        let (1, v) = f.ok()? else { continue };
        let mut source = None;
        let mut platform = None;
        for of in Reader::new(v.bytes().ok()?) {
            match of.ok()? {
                (3, s) => {
                    let mut identifier = Vec::new();
                    let mut attrs = BTreeMap::new();
                    for sf in Reader::new(s.bytes().ok()?) {
                        match sf.ok()? {
                            (1, x) => identifier = text(x)?,
                            (2, e) => {
                                let (mut k, mut val) = (Vec::new(), Vec::new());
                                for ef in Reader::new(e.bytes().ok()?) {
                                    match ef.ok()? {
                                        (1, x) => k = text(x)?,
                                        (2, x) => val = text(x)?,
                                        _ => {}
                                    }
                                }
                                attrs.insert(k, val);
                            }
                            _ => {}
                        }
                    }
                    source = Some((identifier, attrs));
                }
                (10, p) => {
                    let mut out = Platform::default();
                    for pf in Reader::new(p.bytes().ok()?) {
                        match pf.ok()? {
                            (1, x) => out.architecture = text(x)?,
                            (2, x) => out.os = text(x)?,
                            (3, x) => out.variant = text(x)?,
                            (4, x) => out.os_version = text(x)?,
                            (5, x) => out.os_features.push(text(x)?),
                            _ => {}
                        }
                    }
                    platform = Some(out);
                }
                _ => {}
            }
        }
        if let Some((identifier, attrs)) = source {
            return Some(Op {
                inputs: Vec::new(),
                kind: OpKind::Source { identifier, attrs },
                platform,
            });
        }
    }
    None
}

/// An op's tree before it ran and after (D55's guard): a command's root mount's input
/// and output; a file op's base and its output. None for scratch before.
fn before_after(op: &Op, k: usize) -> Option<(Option<Input>, Input)> {
    match &op.kind {
        OpKind::Exec { mounts, .. } => {
            let root = mounts.iter().find(|m| m.dest == b"/")?;
            let before = usize::try_from(root.input)
                .ok()
                .and_then(|i| op.inputs.get(i).copied());
            Some((
                before,
                Input {
                    op: k,
                    index: root.output,
                },
            ))
        }
        OpKind::File { actions } => {
            let base = actions.first()?.input;
            let before = usize::try_from(base).ok().and_then(|i| op.inputs.get(i).copied());
            Some((before, Input { op: k, index: 0 }))
        }
        _ => None,
    }
}

/// D55's isolation checks of an Agentfile's build, run by BuildKit in the frontend's own
/// image (check.rs): each guarded step's writes and the image's domains, every check one
/// step whose report the frontend reads back, all solved at once; the first finding in
/// the definition's order fails the build, a step's on its lines.
fn check_domains<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    me: Option<Op>,
    plan_def: &Definition,
    domains: &[plan::DomainDir],
    carried: &Carried<'_>,
    entry: &Entrypoint,
) -> Result<(), Failure> {
    let Some(me) = me else {
        return Err(Failure::new(
            "shards' frontend checks an Agentfile's domains in its own image, which BuildKit gave no definition of (/run/config/buildkit/metadata/frontend.bin)",
        ));
    };
    let platform = me.platform.clone();
    let mut def = plan_def.clone();
    let image = def
        .root
        .ok_or_else(|| Failure::new("an Agentfile's image of nothing"))?;
    def.ops.push(me);
    def.metadata.push(Meta::default());
    let me = Input {
        op: def.ops.len() - 1,
        index: 0,
    };
    // Each check: its report's name, its op, the step a finding is said of (shards build
    // names the step its guard refuses, as its vertex: the step's own name), and the lines
    // it is said on.
    let mut checks: Vec<Check> = Vec::new();
    let add = |def: &mut Definition, spec: check::Spec, before: Option<Input>, target: Input, shown: &str| {
        let mut inputs = vec![me];
        let mut mounts = vec![shards_dockerfile::llb::OpMount {
            input: 0,
            selector: Vec::new(),
            dest: b"/".to_vec(),
            output: -1,
            readonly: true,
            kind: shards_dockerfile::llb::OpMountKind::Bind,
        }];
        let mount = |dest: &[u8], from: Option<Input>, inputs: &mut Vec<Input>| {
            let input = match from {
                Some(i) => {
                    inputs.push(i);
                    inputs.len() as i64 - 1
                }
                None => -1,
            };
            shards_dockerfile::llb::OpMount {
                input,
                selector: Vec::new(),
                dest: dest.to_vec(),
                output: -1,
                readonly: true,
                kind: shards_dockerfile::llb::OpMountKind::Bind,
            }
        };
        if spec.guard.is_some() {
            mounts.push(mount(b"/after", Some(target), &mut inputs));
            mounts.push(mount(b"/before", before, &mut inputs));
        } else {
            mounts.push(mount(b"/target", Some(target), &mut inputs));
        }
        mounts.push(shards_dockerfile::llb::OpMount {
            input: -1,
            selector: Vec::new(),
            dest: b"/out".to_vec(),
            output: 0,
            readonly: false,
            kind: shards_dockerfile::llb::OpMountKind::Bind,
        });
        let process = shards_dockerfile::llb::Process {
            args: vec![
                b"/shards".to_vec(),
                b"frontend".to_vec(),
                b"check".to_vec(),
                spec.json().into_bytes(),
            ],
            cwd: b"/".to_vec(),
            ..Default::default()
        };
        def.ops.push(Op {
            inputs,
            kind: OpKind::Exec {
                process: Box::new(process),
                mounts,
                network: shards_dockerfile::llb::NetMode::None,
                security: shards_dockerfile::llb::Security::Sandbox,
                secret_env: Vec::new(),
                devices: Vec::new(),
            },
            platform: platform.clone(),
        });
        let mut meta = Meta::default();
        meta.description.insert(
            b"llb.customname".to_vec(),
            format!("[internal] {shown}").into_bytes(),
        );
        def.metadata.push(meta);
        def.ops.len() - 1
    };
    for (k, (op, md)) in plan_def.ops.iter().zip(&plan_def.metadata).enumerate() {
        if !md.description.contains_key(plan::GUARD) {
            continue;
        }
        let Some((before, after)) = before_after(op, k) else {
            continue;
        };
        let report = format!("guard-{k}");
        let spec = check::Spec {
            domains: domains.to_vec(),
            guard: Some(md.description.get(plan::OWN).cloned()),
            report: report.clone(),
        };
        let at = add(
            &mut def,
            spec,
            before,
            after,
            "check a step's writes against the domains",
        );
        checks.push(Check {
            report,
            at,
            step: md.description.get(&b"llb.customname"[..]).cloned(),
            locations: md.locations.clone(),
        });
    }
    let spec = check::Spec {
        domains: domains.to_vec(),
        guard: None,
        report: "image".into(),
    };
    let at = add(&mut def, spec, None, image, "check the image's domains");
    checks.push(Check {
        report: "image".into(),
        at,
        step: None,
        locations: Vec::new(),
    });
    // Every check's report, side by side in one tree.
    let outputs: Vec<Input> = checks.iter().map(|c| Input { op: c.at, index: 0 }).collect();
    def.root = Some(match outputs.as_slice() {
        [one] => *one,
        _ => {
            def.ops.push(Op {
                inputs: outputs,
                kind: OpKind::Merge,
                platform: None,
            });
            def.metadata.push(Meta::default());
            Input {
                op: def.ops.len() - 1,
                index: 0,
            }
        }
    });
    let m = pb::definition(&def, carried).ok_or_else(|| Failure::new("failed to marshal LLB definition"))?;
    let r = g.solve(&m, &[], true)?.unwrap_or_default();
    for c in &checks {
        let finding = g.read_file(&r, &c.report).map_err(ReadError::failure)?;
        if !finding.is_empty() {
            let finding = String::from_utf8_lossy(&finding);
            let said = match &c.step {
                Some(step) => format!("{}: {finding}", String::from_utf8_lossy(step)),
                None => finding.into_owned(),
            };
            return Err(entry.wrap(Failure::new(said), &c.locations));
        }
    }
    Ok(())
}

/// One of an Agentfile's checks, as BuildKit is asked to run it: its report's name, its
/// op, the step a finding is said of, and the lines it is said on.
struct Check {
    report: String,
    at: usize,
    step: Option<Vec<u8>>,
    locations: Vec<shards_dockerfile::instructions::Location>,
}

/// Each OSI artifact a build context gave (D113) laid into `def` in its source's place: the
/// step of the frontend's own image that lays its content out, from the layout's blobs,
/// as its source was named.
fn lay_artifacts<R: Read, W: Write>(g: &Gateway<'_, R, W>, def: &mut Definition) -> Result<(), Failure> {
    // From the last: a step's inputs go in before it, as a definition's every op follows
    // its inputs, and the ops before are left where they are.
    for k in (0..def.ops.len()).rev() {
        let laid = match def.ops.get(k) {
            Some(Op {
                kind: OpKind::Source { identifier, .. },
                ..
            }) if identifier.starts_with(b"osi-artifact://") => {
                g.artifacts.borrow().get(identifier).cloned().ok_or_else(|| {
                    Failure::new(format!("{}: not resolved", String::from_utf8_lossy(identifier)))
                })?
            }
            _ => continue,
        };
        let Some(((exec, _), rest)) = laid.split_last() else {
            continue;
        };
        let n = rest.len();
        for op in &mut def.ops {
            for i in &mut op.inputs {
                if i.op >= k {
                    i.op += n;
                }
            }
        }
        if let Some(r) = &mut def.root
            && r.op >= k
        {
            r.op += n;
        }
        for (j, (op, meta)) in rest.iter().enumerate() {
            let mut op = op.clone();
            for i in &mut op.inputs {
                i.op += k;
            }
            def.ops.insert(k + j, op);
            def.metadata.insert(k + j, meta.clone());
        }
        let mut exec = exec.clone();
        for i in &mut exec.inputs {
            i.op += k;
        }
        if let Some(op) = def.ops.get_mut(k + n) {
            *op = exec;
        }
    }
    Ok(())
}

/// A solved tree as a skills step reads it, through the gateway: its directories'
/// entries (ReadDir), a regular file's bytes (ReadFile, bounded as dockerui bounds a
/// read), no symlink followed.
struct GatewayTree<'g, 'c, R, W> {
    g: &'g Gateway<'c, R, W>,
    r: Ref,
    dirs: BTreeMap<Vec<u8>, Vec<(Vec<u8>, skills::EntryKind)>>,
}

impl<R: Read, W: Write> GatewayTree<'_, '_, R, W> {
    /// Go's os.FileMode type bits: a directory, else a regular file where none is set.
    fn kind(mode: u32) -> skills::EntryKind {
        const DIR: u32 = 1 << 31;
        const TYPE: u32 = DIR | 1 << 27 | 1 << 26 | 1 << 25 | 1 << 24 | 1 << 21 | 1 << 19;
        if mode & DIR != 0 {
            skills::EntryKind::Dir
        } else if mode & TYPE == 0 {
            skills::EntryKind::File
        } else {
            skills::EntryKind::Other
        }
    }

    fn dir(&mut self, path: &[u8]) -> Result<Vec<(Vec<u8>, skills::EntryKind)>, String> {
        if let Some(d) = self.dirs.get(path) {
            return Ok(d.clone());
        }
        let shown = if path.is_empty() {
            "/".to_string()
        } else {
            String::from_utf8_lossy(path).into_owned()
        };
        let list = self
            .g
            .client
            .borrow_mut()
            .read_dir(&self.r.id, &shown, "")
            .map_err(|e| Failure::call(e).message)?;
        let mut out: Vec<(Vec<u8>, skills::EntryKind)> = list
            .into_iter()
            .map(|s| {
                let name = s.path.rsplit('/').next().unwrap_or_default().as_bytes().to_vec();
                (name, Self::kind(s.mode))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        self.dirs.insert(path.to_vec(), out.clone());
        Ok(out)
    }
}

impl<R: Read, W: Write> skills::Fetched for GatewayTree<'_, '_, R, W> {
    fn entries(&mut self) -> Result<Vec<(Vec<u8>, skills::EntryKind)>, String> {
        self.dir(b"")
    }

    fn lookup(&mut self, path: &[u8]) -> Result<skills::Found, String> {
        let (parent, name) = match path.iter().rposition(|&b| b == b'/') {
            Some(i) => (
                path.get(..i).unwrap_or_default(),
                path.get(i + 1..).unwrap_or_default(),
            ),
            None => (b"".as_slice(), path),
        };
        let Some(kind) = self.dir(parent)?.iter().find(|(n, _)| n == name).map(|(_, k)| *k) else {
            return Ok(skills::Found::Missing);
        };
        if kind != skills::EntryKind::File {
            return Ok(skills::Found::Other);
        }
        let shown = String::from_utf8_lossy(path).into_owned();
        self.g
            .read_file(&self.r, &shown)
            .map(skills::Found::File)
            .map_err(|e| e.failure().message)
    }
}

/// Each of shards' own skills steps (D54) made a file op BuildKit runs: the tree it checks
/// solved first, read through the gateway, checked as `shards build` checks it, and laid
/// out by the copies that check gives; a refusal the build's error, on the SKILL's lines.
fn lay_skills<R: Read, W: Write>(
    g: &Gateway<'_, R, W>,
    def: &mut Definition,
    carried: &Carried<'_>,
    entry: &Entrypoint,
) -> Result<(), Failure> {
    for k in 0..def.ops.len() {
        let (came_as, input) = match def.ops.get(k) {
            Some(Op {
                kind: OpKind::Skills { name },
                inputs,
                ..
            }) => (
                String::from_utf8_lossy(name).into_owned(),
                inputs
                    .first()
                    .copied()
                    .ok_or_else(|| Failure::new("a skills step without what it checks"))?,
            ),
            _ => continue,
        };
        let sub = closure(def, input)
            .ok_or_else(|| Failure::new("a skills step's input is not in its definition"))?;
        let m =
            pb::definition(&sub, carried).ok_or_else(|| Failure::new("failed to marshal LLB definition"))?;
        let r = g.solve(&m, &[], true)?.unwrap_or_default();
        let mut tree = GatewayTree {
            g,
            r,
            dirs: BTreeMap::new(),
        };
        let locations = def
            .metadata
            .get(k)
            .map(|m| m.locations.clone())
            .unwrap_or_default();
        let actions =
            skills::layout_of(&mut tree, &came_as).map_err(|e| entry.wrap(Failure::new(e), &locations))?;
        if let Some(op) = def.ops.get_mut(k) {
            op.kind = OpKind::File { actions };
        }
    }
    Ok(())
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
    const SERVER: &[u8] = include_bytes!("../../../gateway/testdata/dockerfile-1.server.bin");
    const CLIENT: &[u8] = include_bytes!("../../../gateway/testdata/dockerfile-1.client.bin");

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
        let mut env = Env::read(vars).unwrap();
        // docker/dockerfile:1 ran on arm64 for the capture: its result's platform, asked
        // for none, is its own (dockerui's platforms.DefaultSpec()), on every host.
        env.own = Platform::new("linux", "arm64");
        env
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

    /// A BuildKit that answers each call, stream by stream, as it is told: the server's half
    /// of a connection, HTTP/2 and gRPC as grpc-go writes them, each answer a message or a
    /// status (trailers-only).
    fn fake_server(answers: &[Result<Vec<u8>, (u32, &str)>]) -> Vec<u8> {
        use shards_gateway::hpack;
        let frame = |out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, payload: &[u8]| {
            let n = payload.len() as u32;
            out.extend_from_slice(&n.to_be_bytes()[1..]);
            out.push(kind);
            out.push(flags);
            out.extend_from_slice(&stream.to_be_bytes());
            out.extend_from_slice(payload);
        };
        let mut out = Vec::new();
        frame(&mut out, 4, 0, 0, &[]);
        for (i, a) in answers.iter().enumerate() {
            let stream = 2 * i as u32 + 1;
            match a {
                Ok(message) => {
                    let head = hpack::encode(&[(":status", "200"), ("content-type", "application/grpc")]);
                    frame(&mut out, 1, 0x4, stream, &head);
                    let mut data = vec![0u8];
                    data.extend_from_slice(&(message.len() as u32).to_be_bytes());
                    data.extend_from_slice(message);
                    frame(&mut out, 0, 0, stream, &data);
                    frame(&mut out, 1, 0x5, stream, &hpack::encode(&[("grpc-status", "0")]));
                }
                Err((code, message)) => {
                    let code = code.to_string();
                    let head = hpack::encode(&[
                        (":status", "200"),
                        ("content-type", "application/grpc"),
                        ("grpc-status", &code),
                        ("grpc-message", message),
                    ]);
                    frame(&mut out, 1, 0x5, stream, &head);
                }
            }
        }
        out
    }

    /// A SolveResponse holding the ref `id`.
    fn solved(id: &str) -> Vec<u8> {
        let mut r = shards_gateway::wire::Writer::default();
        r.string(1, id);
        let mut result = shards_gateway::wire::Writer::default();
        result.message(3, &r.0);
        let mut w = shards_gateway::wire::Writer::default();
        w.message(3, &result.0);
        w.0
    }

    /// fsutil's Stat of `path`, mode `mode`, `size` bytes.
    fn stat(path: &str, mode: u32, size: u64) -> Vec<u8> {
        let mut s = shards_gateway::wire::Writer::default();
        s.string(1, path);
        s.uint(2, u64::from(mode));
        s.uint(5, size);
        s.0
    }

    /// A ReadDirResponse of `entries`.
    fn listed(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut w = shards_gateway::wire::Writer::default();
        for e in entries {
            w.message(1, e);
        }
        w.0
    }

    /// A StatFileResponse of `stat`, and a ReadFileResponse of `data`.
    fn stated(stat: Vec<u8>) -> Vec<u8> {
        let mut w = shards_gateway::wire::Writer::default();
        w.message(1, &stat);
        w.0
    }

    fn read(data: &[u8]) -> Vec<u8> {
        let mut w = shards_gateway::wire::Writer::default();
        w.bytes(1, data);
        w.0
    }

    const DIR: u32 = 1 << 31 | 0o755;

    /// A gateway over a fake BuildKit's `answers`, with the frontend capabilities BuildKit
    /// v0.28.1 said it has in the capture, and grpcclient's LLB defaults.
    fn gateway_over(
        server: Vec<u8>,
        out: &mut Vec<u8>,
        f: impl FnOnce(&Gateway<'_, std::io::Cursor<Vec<u8>>, &mut Vec<u8>>),
    ) {
        let mut client = Client::new(std::io::Cursor::new(server), out).unwrap();
        let frontend_caps = [
            "frontend.caps",
            "frontend.inputs",
            "gateway.evaluate",
            "gateway.exec",
            "gateway.solve.evaluate",
            "gateway.solve.metadata",
            "gateway.warnings",
            "proto.refarray",
            "readdir",
            "readfile",
            "return",
            "returnmap",
            "solve.base",
            "source.metaresolver",
            "statfile",
        ]
        .iter()
        .map(|id| gw::Cap {
            id: (*id).to_string(),
            enabled: true,
            disabled_reason_msg: String::new(),
        })
        .collect();
        // The LLB capabilities BuildKit v0.28.1 said it has in the capture.
        let llb_caps = "cache.azblob,cache.gha,cache.s3,constraints,diffop,exec.cgroup,exec.meta.base,exec.meta.cdi,exec.meta.cgroup.parent,exec.meta.network,exec.meta.proxyenv,exec.meta.security,exec.meta.security.devices.v1,exec.meta.setsdefaultpath,exec.meta.ulimit,exec.mount.bind,exec.mount.bind.readwrite-nooutput,exec.mount.cache,exec.mount.cache.content,exec.mount.cache.sharing,exec.mount.secret,exec.mount.selector,exec.mount.ssh,exec.mount.tmpfs,exec.mount.tmpfs.size,exec.secretenv,exec.validexitcode,exporter.image.annotations,exporter.image.attestations,exporter.multiple,exporter.session,exporter.sourcedateepoch,file.base,file.copy.alwaysreplaceexistingdestpaths,file.copy.includeexcludepatterns,file.copy.requiredpaths,file.rm.nofollowsymlink,file.rm.wildcard,file.symlink.create,gc.freespacefilter,history.filter,mergeop,meta.description,meta.exportcache,meta.ignorecache,platform,soruce.http.uidgid,source.buildop.llbfilename,source.git,source.git.checksum,source.git.fullurl,source.git.httpauth,source.git.keepgitdir,source.git.knownsshhosts,source.git.mountsshsock,source.git.signatureverify,source.git.skipsubmodules,source.git.subdir,source.http,source.http.auth,source.http.checksum,source.http.header,source.http.perm,source.http.signatureverify,source.image,source.image.checksum,source.image.layerlimit,source.image.resolvemode,source.imageblob,source.local,source.local.differ,source.local.excludepatterns,source.local.followpaths,source.local.includepatterns,source.local.metadatatransfer,source.local.sessionid,source.local.sharedkeyhint,source.local.unique,source.ocilayout,source.policy,source.policy.session"
            .split(',')
            .map(|id| gw::Cap {
                id: id.to_string(),
                enabled: true,
                disabled_reason_msg: String::new(),
            })
            .collect();
        let caps = Caps::of(&Pong {
            frontend_caps,
            llb_caps,
            ..Pong::default()
        });
        let g = Gateway {
            client: RefCell::new(&mut client),
            caps,
            env: captured_env(),
            opts: BTreeMap::new(),
            failed: RefCell::new(None),
            resolved: RefCell::new(BTreeMap::new()),
            dockerignore: RefCell::new(None),
            warn_into: RefCell::new(None),
            artifacts: RefCell::new(BTreeMap::new()),
            me: Some(my_image()),
        };
        f(&g);
    }

    /// A definition of the context, a SKILL's skills step over it, as the planner makes one.
    fn skills_definition() -> Definition {
        let mut meta = Meta::default();
        meta.locations.push(vec![(2, 2)]);
        Definition {
            ops: vec![
                Op {
                    inputs: Vec::new(),
                    kind: OpKind::Source {
                        identifier: b"local://context".to_vec(),
                        attrs: BTreeMap::from([(b"local.session".to_vec(), b"s".to_vec())]),
                    },
                    platform: None,
                },
                Op {
                    inputs: vec![Input { op: 0, index: 0 }],
                    kind: OpKind::Skills {
                        name: b"my-skill".to_vec(),
                    },
                    platform: None,
                },
            ],
            metadata: vec![Meta::default(), meta],
            root: Some(Input { op: 1, index: 0 }),
        }
    }

    fn entry() -> Entrypoint {
        let def = local_definition("dockerfile", &["Agentfile"], "s", "dockerfile", "[internal] load");
        Entrypoint {
            filename: "Agentfile".into(),
            data: b"FROM scratch\nSKILL ./my-skill\n".to_vec(),
            definition: pb::definition(&def, &Carried::default()).unwrap(),
            definition_json: pb::definition_json(&def, &Carried::default()).unwrap(),
        }
    }

    /// A skills step becomes the file op `shards build` lays its skills with: its tree
    /// solved and read through the gateway, checked as shards build checks it.
    #[test]
    fn skills_are_laid_out_through_the_gateway_as_shards_build_lays_them() {
        let md = b"---\nname: my-skill\ndescription: Does a thing.\n---\nBody.\n";
        let server = fake_server(&[
            Ok(solved("r1")),
            Ok(listed(&[stat("my-skill", DIR, 0)])),
            Ok(listed(&[stat("SKILL.md", 0o644, md.len() as u64)])),
            Ok(stated(stat("my-skill/SKILL.md", 0o644, md.len() as u64))),
            Ok(read(md)),
        ]);
        let mut out = Vec::new();
        gateway_over(server, &mut out, |g| {
            let mut def = skills_definition();
            lay_skills(g, &mut def, &Carried::default(), &entry()).unwrap();
            let Some(Op {
                kind: OpKind::File { actions },
                inputs,
                ..
            }) = def.ops.get(1)
            else {
                panic!("{:?}", def.ops.get(1));
            };
            assert_eq!(inputs, &vec![Input { op: 0, index: 0 }]);
            assert_eq!(actions.len(), 1);
            let shards_dockerfile::llb::OpActionKind::Copy {
                src,
                dest,
                dir_copy_contents,
                ..
            } = &actions[0].action
            else {
                panic!("{actions:?}");
            };
            assert_eq!(
                (src.as_slice(), dest.as_slice(), *dir_copy_contents),
                (b"/my-skill".as_slice(), b"/my-skill".as_slice(), true)
            );
        });
    }

    /// Where BuildKit cannot evaluate a solve, a stat of its result's root makes it, as
    /// grpcclient falls back; where it can, it is asked to.
    #[test]
    fn an_evaluated_solve_falls_back_to_a_stat_as_grpcclient_does() {
        let def = local_definition("context", &[".dockerignore"], "s", "k", "[internal] load");
        let m = pb::definition(&def, &Carried::default()).unwrap();
        // Without gateway.solve.evaluate: Solve, then StatFile of ".".
        let server = fake_server(&[Ok(solved("r1")), Ok(stated(stat(".", DIR, 0)))]);
        let mut out = Vec::new();
        let mut client = Client::new(std::io::Cursor::new(server), &mut out).unwrap();
        let g = Gateway {
            client: RefCell::new(&mut client),
            caps: Caps::of(&Pong::default()),
            env: captured_env(),
            opts: BTreeMap::new(),
            failed: RefCell::new(None),
            resolved: RefCell::new(BTreeMap::new()),
            dockerignore: RefCell::new(None),
            warn_into: RefCell::new(None),
            artifacts: RefCell::new(BTreeMap::new()),
            me: Some(my_image()),
        };
        assert_eq!(g.solve(&m, &[], true).unwrap().unwrap().id, "r1");
        drop(g);
        let asked = messages(&out);
        assert_eq!(asked.len(), 2, "{asked:?}");
        // The Solve did not ask BuildKit to evaluate (field 14 absent).
        assert!(!fields(&asked[&1][0]).iter().any(|(tag, _)| tag >> 3 == 14));
        // With it: one Solve, evaluated.
        let mut out = Vec::new();
        gateway_over(fake_server(&[Ok(solved("r1"))]), &mut out, |g| {
            assert_eq!(g.solve(&m, &[], true).unwrap().unwrap().id, "r1");
        });
        let asked = messages(&out);
        assert_eq!(asked.len(), 1);
        assert!(
            fields(&asked[&1][0])
                .iter()
                .any(|(tag, v)| tag >> 3 == 14 && v == &[1])
        );
    }

    /// A skill the check refuses fails the build with the check's words, its excerpt the
    /// SKILL's line.
    #[test]
    fn a_refused_skill_fails_the_build_on_its_line() {
        let md = b"---\nname: Not Valid\n---\n";
        let server = fake_server(&[
            Ok(solved("r1")),
            Ok(listed(&[stat("my-skill", DIR, 0)])),
            Ok(listed(&[stat("SKILL.md", 0o644, md.len() as u64)])),
            Ok(stated(stat("my-skill/SKILL.md", 0o644, md.len() as u64))),
            Ok(read(md)),
        ]);
        let mut out = Vec::new();
        gateway_over(server, &mut out, |g| {
            let mut def = skills_definition();
            let f = lay_skills(g, &mut def, &Carried::default(), &entry()).unwrap_err();
            assert!(f.message.starts_with("skill my-skill: "), "{}", f.message);
            assert_eq!(f.details.len(), 1);
            assert_eq!(f.details[0].0, SOURCE_DETAIL);
            let detail = String::from_utf8(f.details[0].1.clone()).unwrap();
            assert!(
                detail.ends_with(r#""ranges":[{"start":{"line":2},"end":{"line":2}}]}"#),
                "{detail}"
            );
        });
    }

    /// The frontend's own image, as a source op of its definition.
    fn my_image() -> Op {
        Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: b"docker-image://127.0.0.1:15113/shards-d113-frontend:1@sha256:0000000000000000000000000000000000000000000000000000000000000001".to_vec(),
                attrs: BTreeMap::from([(b"image.recordtype".to_vec(), b"frontend".to_vec())]),
            },
            platform: Some(Platform::new("linux", "arm64")),
        }
    }

    /// The source op of the definition BuildKit mounts for its frontend reads back as it
    /// was written.
    #[test]
    fn the_frontends_own_image_reads_from_its_definition() {
        let def = Definition {
            ops: vec![my_image()],
            metadata: vec![Meta::default()],
            root: Some(Input { op: 0, index: 0 }),
        };
        let m = pb::definition(&def, &Carried::default()).unwrap();
        assert_eq!(source_op_of(&m.bytes), Some(my_image()));
    }

    /// An Agentfile's checks: each guarded step's writes and the image's domains, steps of
    /// the frontend's own image, solved at once; a finding fails the build on its step's
    /// lines, the image's on none.
    #[test]
    fn an_agentfiles_domains_are_checked_in_the_frontends_image() {
        let domains = vec![plan::DomainDir {
            name: b"a".to_vec(),
            harness: false,
            dir: b"/agents/a".to_vec(),
        }];
        // The context, a guarded RUN on it, as the planner marks one.
        let mut guarded = Meta::default();
        guarded.description.insert(plan::GUARD.to_vec(), b"1".to_vec());
        guarded.description.insert(
            b"llb.customname".to_vec(),
            b"[2/2] RUN echo > /agents/a/x".to_vec(),
        );
        guarded.locations.push(vec![(2, 2)]);
        let def = Definition {
            ops: vec![
                Op {
                    inputs: Vec::new(),
                    kind: OpKind::Source {
                        identifier: b"docker-image://docker.io/library/alpine:3.20".to_vec(),
                        attrs: BTreeMap::new(),
                    },
                    platform: Some(Platform::new("linux", "arm64")),
                },
                Op {
                    inputs: vec![Input { op: 0, index: 0 }],
                    kind: OpKind::Exec {
                        process: Box::new(shards_dockerfile::llb::Process {
                            args: vec![
                                b"/bin/sh".to_vec(),
                                b"-c".to_vec(),
                                b"echo > /agents/a/x".to_vec(),
                            ],
                            ..Default::default()
                        }),
                        mounts: vec![shards_dockerfile::llb::OpMount {
                            input: 0,
                            selector: Vec::new(),
                            dest: b"/".to_vec(),
                            output: 0,
                            readonly: false,
                            kind: shards_dockerfile::llb::OpMountKind::Bind,
                        }],
                        network: shards_dockerfile::llb::NetMode::Sandbox,
                        security: shards_dockerfile::llb::Security::Sandbox,
                        secret_env: Vec::new(),
                        devices: Vec::new(),
                    },
                    platform: Some(Platform::new("linux", "arm64")),
                },
            ],
            metadata: vec![Meta::default(), guarded],
            root: Some(Input { op: 1, index: 0 }),
        };
        let said = "it writes /agents/a/x in the agent a's domain, which only that domain's own directives write (AGENTFILE_ARCH.md §9.2)";
        let server = fake_server(&[
            Ok(solved("checks")),
            Ok(stated(stat("guard-1", 0o644, said.len() as u64))),
            Ok(read(said.as_bytes())),
        ]);
        let mut out = Vec::new();
        gateway_over(server, &mut out, |g| {
            let f = check_domains(g, Some(my_image()), &def, &domains, &Carried::default(), &entry())
                .unwrap_err();
            // Said of the step, as shards build says its guard's refusal.
            assert_eq!(f.message, format!("[2/2] RUN echo > /agents/a/x: {said}"));
            let detail = String::from_utf8(f.details[0].1.clone()).unwrap();
            assert!(
                detail.ends_with(r#""ranges":[{"start":{"line":2},"end":{"line":2}}]}"#),
                "{detail}"
            );
        });
        // What BuildKit was asked: the build's ops, the frontend's image, a check of the
        // RUN and one of the image, each `/shards frontend check`, merged.
        let asked = messages(&out);
        let solve = &asked[&1][0];
        let definition = fields(solve)
            .into_iter()
            .find(|(tag, _)| tag >> 3 == 1)
            .map(|(_, v)| v)
            .unwrap();
        let ops: Vec<Vec<u8>> = fields(&definition)
            .into_iter()
            .filter(|(tag, _)| tag >> 3 == 1)
            .map(|(_, v)| v)
            .collect();
        // alpine, the RUN, the frontend's image, two checks, the merge, the root.
        assert_eq!(ops.len(), 7);
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        assert!(text(&ops[2]).contains("shards-d113-frontend"));
        for (op, report) in [(&ops[3], "guard-1"), (&ops[4], "image")] {
            let t = text(op);
            assert!(
                t.contains("/shards") && t.contains("frontend") && t.contains("check"),
                "{t}"
            );
            assert!(t.contains(&format!("\"report\":\"{report}\"")), "{t}");
        }
        // With no image of its own the frontend refuses rather than skip the checks.
        let mut out = Vec::new();
        gateway_over(fake_server(&[]), &mut out, |g| {
            let f = check_domains(g, None, &def, &domains, &Carried::default(), &entry()).unwrap_err();
            assert!(f.message.contains("frontend.bin"), "{}", f.message);
        });
    }

    /// A resolver of no images: a scratch Agentfile needs none.
    struct NoImages;

    impl Resolver for NoImages {
        fn resolve(&self, name: &[u8], _: &Platform, _: &[u8]) -> Result<Resolved, Vec<u8>> {
            Err([b"no image ".as_slice(), name].concat())
        }

        fn epoch(&self, _: &EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>> {
            Ok(None)
        }
    }

    /// An Agentfile's image is annotated as shards build annotates it (D57): the
    /// normalized Agentfile's digest, its config label's, and its agents' and harnesses'
    /// names in order.
    #[test]
    fn an_agentfiles_manifest_is_annotated_as_shards_build_annotates_it() {
        let opts = Options {
            target_platform: Platform::new("linux", "arm64"),
            build_platforms: vec![Platform::new("linux", "arm64")],
            dialect: Dialect::Agentfile,
            ..Options::default()
        };
        let text = b"FROM scratch\nAGENT zed FROM ./z\nAGENT main FROM ./m\nHARNESS ci FROM ./h\n";
        let planned = plan::plan(text, &opts, &NoImages).unwrap();
        let ann = agentfile_annotations(&planned);
        let digest = planned
            .image
            .config
            .labels
            .get(shards_dockerfile::agentfile::DIGEST_LABEL)
            .unwrap()
            .clone();
        assert_eq!(
            ann,
            vec![
                ("vnd.osi.agentfile.digest".to_string(), digest),
                ("vnd.osi.agentfile.agents".to_string(), b"main,zed".to_vec()),
                ("vnd.osi.agentfile.harnesses".to_string(), b"ci".to_vec()),
            ]
        );
        // A Dockerfile's has none.
        let plain = plan::plan(
            b"FROM scratch\n",
            &Options {
                dialect: Dialect::Dockerfile,
                ..opts
            },
            &NoImages,
        )
        .unwrap();
        assert!(agentfile_annotations(&plain).is_empty());
    }

    /// An OSI artifact named by its reference alone is refused, saying what to give, its
    /// context keyed as a FROM's is (`:latest` and Docker Hub's names in their familiar
    /// form).
    #[test]
    fn an_artifact_without_its_layout_is_refused_saying_what_to_give() {
        let mut out = Vec::new();
        gateway_over(fake_server(&[]), &mut out, |g| {
            let said = |name: &str| {
                String::from_utf8(g.artifact(name.as_bytes(), b"agent", b"").unwrap_err()).unwrap()
            };
            assert_eq!(
                said("ghcr.io/org/agent:1"),
                "ghcr.io/org/agent:1: through BuildKit an OSI artifact comes from an OCI layout given as a build context, BuildKit giving a frontend no artifact's manifest: --build-context ghcr.io/org/agent:1=oci-layout://DIR:TAG, which `oras cp --to-oci-layout ghcr.io/org/agent:1 DIR:TAG` or `skopeo copy --preserve-digests docker://ghcr.io/org/agent:1 oci:DIR:TAG` makes"
            );
            assert!(
                said("docker.io/library/agent:latest").contains("--build-context agent=oci-layout://DIR:TAG")
            );
        });
    }

    /// An OSI artifact from the OCI layout a build context gives (D113): its manifest and
    /// config read from the client's layout by digest and held as `shards build` holds
    /// them, its layer held and laid out by a step of the frontend's own image, which the
    /// build's definition then takes in its source's place.
    #[test]
    fn an_artifact_from_a_layout_is_held_and_laid_out_in_the_frontends_image() {
        let digest_of = |b: &[u8]| osi::sha256_of(b).unwrap();
        let config = br#"{"schemaVersion":1,"name":"main","run":{"command":["bin/run"]}}"#;
        let layer = "sha256:".to_string() + &"1".repeat(64);
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":"application/vnd.osi.agent.v1","config":{{"mediaType":"application/vnd.osi.agent.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.osi.agent.content.v1.tar","digest":"{layer}","size":10240}}]}}"#,
            digest_of(config),
            config.len()
        );
        let m_digest = digest_of(manifest.as_bytes());
        let blob = |id: &str, bytes: &[u8]| {
            [
                Ok(solved(id)),
                Ok(stated(stat("blob", 0o644, bytes.len() as u64))),
                Ok(read(bytes)),
            ]
        };
        let mut answers: Vec<Result<Vec<u8>, (u32, &str)>> = Vec::new();
        answers.extend(blob("m", manifest.as_bytes()));
        answers.extend(blob("c", config));
        answers.extend([
            Ok(solved("osi")),
            Ok(stated(stat("finding", 0o644, 0))),
            Ok(read(b"")),
        ]);
        let mut out = Vec::new();
        gateway_over(fake_server(&answers), &mut out, |g| {
            let r = g
                .artifact_layout(
                    b"reg.example/agent:1",
                    b"agent",
                    b"store1",
                    m_digest.as_bytes(),
                    b"",
                )
                .unwrap();
            assert_eq!(
                r.reference,
                format!("reg.example/agent:1@{m_digest}").into_bytes()
            );
            assert_eq!(r.config, config.to_vec());
            // The build's definition: its source in the layout step's place.
            let mut def = Definition {
                ops: vec![Op {
                    inputs: Vec::new(),
                    kind: OpKind::Source {
                        identifier: [b"osi-artifact://".as_slice(), &r.reference].concat(),
                        attrs: BTreeMap::from([(b"osi.kind".to_vec(), b"agent".to_vec())]),
                    },
                    platform: None,
                }],
                metadata: vec![Meta::default()],
                root: Some(Input { op: 0, index: 0 }),
            };
            lay_artifacts(g, &mut def).unwrap();
            // Its inputs before it, as every op follows its inputs; the root on it.
            assert_eq!(def.root, Some(Input { op: 2, index: 0 }));
            assert!(
                pb::definition(&def, &Carried::default()).is_some(),
                "the definition marshals"
            );
            let Some(Op {
                kind:
                    OpKind::Exec {
                        process,
                        mounts,
                        network,
                        ..
                    },
                inputs,
                ..
            }) = def.ops.get(2)
            else {
                panic!("{:?}", def.ops);
            };
            assert_eq!(
                process.args[..3],
                [b"/shards".to_vec(), b"frontend".to_vec(), b"osi".to_vec()]
            );
            assert_eq!(*network, shards_dockerfile::llb::NetMode::None);
            let dests: Vec<&[u8]> = mounts.iter().map(|m| m.dest.as_slice()).collect();
            assert_eq!(dests, [&b"/"[..], b"/layers/0", b"/out", b"/report"]);
            // Its inputs: the frontend's image and the layer's blob, appended.
            let source_of = |i: &Input| match &def.ops[i.op].kind {
                OpKind::Source { identifier, attrs } => {
                    (String::from_utf8_lossy(identifier).into_owned(), attrs.clone())
                }
                other => panic!("{other:?}"),
            };
            assert!(source_of(&inputs[0]).0.contains("shards-d113-frontend"));
            let (id, attrs) = source_of(&inputs[1]);
            assert_eq!(id, format!("oci-layout+blob://docker.io/library/store1@{layer}"));
            assert_eq!(
                attrs.get(&b"oci.store"[..]).map(Vec::as_slice),
                Some(&b"store1"[..])
            );
            assert_eq!(
                attrs.get(&b"http.filename"[..]).map(Vec::as_slice),
                Some(&b"blob"[..])
            );
        });
        // A manifest that is not the one its digest names is refused.
        let mut answers: Vec<Result<Vec<u8>, (u32, &str)>> = Vec::new();
        answers.extend(blob("m", manifest.as_bytes()));
        let mut out = Vec::new();
        let other = "sha256:".to_string() + &"2".repeat(64);
        gateway_over(fake_server(&answers), &mut out, |g| {
            let e = g
                .artifact_layout(b"reg.example/agent:1", b"agent", b"store1", other.as_bytes(), b"")
                .unwrap_err();
            assert_eq!(
                String::from_utf8(e).unwrap(),
                format!("got digest {m_digest}, expected {other}")
            );
        });
        // An artifact of another kind than the directive names is refused in shards build's
        // words.
        let mut answers: Vec<Result<Vec<u8>, (u32, &str)>> = Vec::new();
        answers.extend(blob("m", manifest.as_bytes()));
        let mut out = Vec::new();
        gateway_over(fake_server(&answers), &mut out, |g| {
            let e = g
                .artifact_layout(
                    b"reg.example/agent:1",
                    b"harness",
                    b"store1",
                    m_digest.as_bytes(),
                    b"",
                )
                .unwrap_err();
            assert_eq!(
                String::from_utf8(e).unwrap(),
                "reg.example/agent:1 is an OSI agent, not an OSI harness"
            );
        });
    }

    /// The image's label lists what the frontend can do, so that BuildKit refuses a build
    /// asking more before it runs it.
    #[test]
    fn the_images_caps_label_is_what_the_frontend_does() {
        let dockerfile = include_str!("../../../../scripts/frontend/Dockerfile");
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
