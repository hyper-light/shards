//! Build policies (D101), as buildx v0.37.1 sets them up (build/opt.go
//! configureSourcePolicy, build/policy_loader.go) and asks them (policy/validate.go),
//! and as BuildKit v0.28.1 heeds them (solver/llbsolver/policy.go): the Dockerfile's own
//! `Dockerfile.rego` and each `--policy`, every one a set of Rego modules asked
//! `data.docker.decision` of each source the build loads. What a decision needs of a
//! source that is not known yet, partial evaluation finds; the source's metadata is
//! resolved for it, and the policy asked again. One policy's denial stops the build in
//! BuildKit's words; every policy must allow a source for the build to load it.

mod eval;
mod github;
mod gitobject;
mod input;
mod provenance;
mod signatures;
mod snappy;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use shards_cmdline::buildflags::{LogLevel, PolicyConfig};
use shards_dockerfile::platform::{self, Platform};
use shards_rego::ast::{Module, Term, TermValue};
use shards_rego::compile::{Compiler, Function};
use shards_rego::eval::{Host, HostError, Machine, Program, eval_query, partial_query};
use shards_rego::types::Type;
use shards_rego::value::Value;

pub use input::Env;
use input::Input;

/// buildx's own policy module, beside every policy's (policy/builtins.rego).
const BUILTIN_MODULE: (&str, &str) = (
    "builtin/buildx_defaults.rego",
    include_str!("../../../../rego/src/buildx_defaults.rego"),
);

/// buildx's default policy (policy/default.rego), loaded as a policy file of this name.
const DEFAULT_POLICY: (&str, &str) = (
    "buildx_default_policy.rego",
    include_str!("../../../../rego/src/buildx_default_policy.rego"),
);

/// DefaultPolicyEnabled: BUILDX_DEFAULT_POLICY, where strconv.ParseBool takes it.
pub fn default_policy_enabled() -> bool {
    std::env::var("BUILDX_DEFAULT_POLICY")
        .ok()
        .and_then(|v| shards_cmdline::go::parse_bool(&v).ok())
        == Some(true)
}

/// How often a policy may ask for more of a source before it decides
/// (maxResolveIterations), and how often BuildKit asks again for one source.
const MAX_RESOLVES: usize = 10;

/// A policy file: its contents where they are known already, else its name, read from
/// the build's context or (`cwd://`) the working directory; `optional` where its absence
/// drops it (`policyFileSpec`).
#[derive(Debug, Clone)]
pub struct FileSpec {
    pub filename: String,
    pub optional: bool,
    pub data: Option<Vec<u8>>,
}

/// A policy as configured (`policyOpt`): its files, the context its names are read from,
/// and how it is evaluated.
#[derive(Debug, Clone, Default)]
pub struct Opt {
    pub files: Vec<FileSpec>,
    pub context_dir: Option<PathBuf>,
    pub strict: bool,
    pub log_level: Option<LogLevel>,
    /// Not asked for its caps as the build begins (the default policy's).
    pub skip_caps: bool,
}

/// `withPolicyConfig`: the Dockerfile's own policy, then each `--policy`'s, as the flags
/// combine them: `disabled` alone turns every policy off; `reset` drops those before it;
/// a value naming no file sets `strict` and `log-level` of the policy before it, or of
/// the next where there is none yet.
pub fn with_config(default: Opt, configs: &[PolicyConfig]) -> Result<Vec<Opt>, String> {
    if configs.is_empty() {
        return Ok(if default.files.is_empty() {
            Vec::new()
        } else {
            vec![default]
        });
    }
    if let Some(cfg) = configs.iter().find(|c| c.disabled) {
        let alone = !cfg.reset && cfg.strict.is_none() && cfg.log_level.is_none() && cfg.files.is_empty();
        if !alone || configs.len() > 1 {
            return Err("disabled policy cannot be combined with other policy flags".into());
        }
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    if !default.files.is_empty() {
        out.push(default.clone());
    }
    let mut last = PolicyConfig::default();
    for cfg in configs {
        if cfg.reset {
            out.clear();
        }
        if cfg.files.is_empty() {
            match out.last_mut() {
                None => last = cfg.clone(),
                Some(tail) => {
                    if let Some(s) = cfg.strict {
                        tail.strict = s;
                    }
                    if cfg.log_level.is_some() {
                        tail.log_level = cfg.log_level;
                    }
                }
            }
            continue;
        }
        let mut opt = Opt {
            files: cfg
                .files
                .iter()
                .map(|f| FileSpec {
                    filename: f.clone(),
                    optional: false,
                    data: None,
                })
                .collect(),
            context_dir: default.context_dir.clone(),
            ..Opt::default()
        };
        for c in [&last, cfg] {
            if let Some(s) = c.strict {
                opt.strict = s;
            }
            if c.log_level.is_some() {
                opt.log_level = c.log_level;
            }
        }
        out.push(opt);
    }
    Ok(out)
}

/// Where a policy's names are read: its build's context, or the working directory for
/// a name with `cwd://` (policyPathFS).
#[derive(Debug, Clone)]
struct Fs {
    context_dir: Option<PathBuf>,
    cwd: PathBuf,
}

/// Why a policy file could not be had: it is not there, or reading it failed.
enum Missing {
    NotFound,
    Failed(String),
}

impl Fs {
    /// The root a name is read in and the name in it (policyPathFSRef.resolve): a
    /// `cwd://` name in the working directory; any other in the context, an absolute one
    /// inside the context as its path there (normalizeLocalPolicyPath).
    fn place(&self, name: &str) -> Result<Option<(PathBuf, String)>, String> {
        if name.is_empty() {
            return Err("policy filename is empty".into());
        }
        if let Some(v) = name.strip_prefix("cwd://") {
            if v.is_empty() {
                return Err(format!(
                    "invalid policy filename {}",
                    shards_cmdline::go::quote(name)
                ));
            }
            return Ok(Some((self.cwd.clone(), clean(v))));
        }
        let Some(context) = &self.context_dir else {
            return Ok(None);
        };
        // A context named relative to the working directory is taken as the absolute path
        // it names, so that an absolute name inside it is found: buildx's filepath.Rel
        // refuses to relate an absolute name to a relative context, and the name is then
        // read as the invalid fs.FS name it is.
        let absolute = if context.is_absolute() {
            context.clone()
        } else {
            self.cwd.join(context)
        };
        let target = match within(&absolute, name) {
            Some(rel) => rel,
            None => clean(name),
        };
        Ok(Some((context.clone(), target)))
    }

    /// loadPolicyData: the file, found as `os.Root.FS()` finds it.
    fn read(&self, name: &str) -> Result<Vec<u8>, Missing> {
        let Some((root, target)) = self.place(name).map_err(Missing::Failed)? else {
            return Err(Missing::NotFound);
        };
        match shards_archive::read_in_root(root.as_os_str().as_encoded_bytes(), &target) {
            Ok(data) => Ok(data),
            Err(shards_archive::RootRead::Stat(e)) if not_found(&e) => Err(Missing::NotFound),
            Err(shards_archive::RootRead::Stat(e)) => {
                Err(Missing::Failed(format!("failed to stat policy file {name}: {e}")))
            }
            Err(shards_archive::RootRead::Read(e)) if not_found(&e) => Err(Missing::NotFound),
            Err(shards_archive::RootRead::Read(e)) => {
                Err(Missing::Failed(format!("failed to read policy file {name}: {e}")))
            }
        }
    }
}

/// isFileNotFoundError: a file not there, or an error that says so.
fn not_found(e: &shards_archive::Error) -> bool {
    let text = e.to_string().to_lowercase();
    e.kind() == shards_archive::Kind::NotFound || text.contains("not found") || text.contains("no such file")
}

/// `path.Clean(filepath.ToSlash(p))`.
fn clean(p: &str) -> String {
    let slashed = if cfg!(windows) {
        p.replace('\\', "/")
    } else {
        p.to_string()
    };
    String::from_utf8_lossy(&shards_dockerfile::go::clean(slashed.as_bytes())).into_owned()
}

/// An absolute `name`'s path inside the absolute directory `dir`, if it is inside:
/// `filepath.Rel` where its answer climbs out of neither.
fn within(dir: &Path, name: &str) -> Option<String> {
    let name = Path::new(name);
    if !name.is_absolute() {
        return None;
    }
    let rel = name.strip_prefix(dir).ok()?;
    let rel = rel.to_string_lossy();
    Some(if rel.is_empty() {
        ".".to_string()
    } else {
        clean(&rel)
    })
}

/// A source as BuildKit names one (pb.SourceOp): its identifier and attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub identifier: String,
    pub attrs: BTreeMap<String, String>,
}

impl Source {
    pub fn new(identifier: impl Into<String>) -> Source {
        Source {
            identifier: identifier.into(),
            attrs: BTreeMap::new(),
        }
    }
}

/// What is known of a source (ResolveSourceMetaResponse).
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub image: Option<ImageMeta>,
    pub git: Option<GitMeta>,
    pub http: Option<HttpMeta>,
}

/// A Git source's metadata (ResolveSourceGitResponse): the ref its name resolves to, the
/// object that ref names (an annotated tag's own), the commit, and their raw objects
/// where asked for.
#[derive(Debug, Clone, Default)]
pub struct GitMeta {
    pub reference: String,
    pub checksum: String,
    pub commit_checksum: String,
    pub commit_object: Option<Vec<u8>>,
    pub tag_object: Option<Vec<u8>>,
}

/// An HTTP source's metadata: its content's digest, and where asked for, the digest of
/// its content and a suffix (ChecksumResponse), and that suffix.
#[derive(Debug, Clone, Default)]
pub struct HttpMeta {
    pub checksum: String,
    pub signature_checksum: Option<(String, Vec<u8>)>,
}

/// An image's metadata: the digest its name resolves to, its config where asked for,
/// and its attestation chain where asked for and it has one.
#[derive(Debug, Clone)]
pub struct ImageMeta {
    pub digest: String,
    pub config: Option<Vec<u8>>,
    pub attestation_chain: Option<AttestationChain>,
}

/// sourceresolver.AttestationChain as buildx reads it: from the image's index (`root`),
/// the attestation manifest beside the manifest its platform resolved to, the signature
/// manifests referring to that, and the blobs read on the way (the index, the
/// attestation and signature manifests and their layers, and the attestations asked
/// for), by digest.
#[derive(Debug, Clone, Default)]
pub struct AttestationChain {
    pub root: String,
    pub attestation_manifest: String,
    pub signature_manifests: Vec<String>,
    pub blobs: BTreeMap<String, (shards_sigstore::image::Descriptor, Vec<u8>)>,
}

/// What a policy asks to know of a source (ResolveSourceMetaRequest), and for which
/// platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetaRequest {
    pub platform: Option<Platform>,
    pub image: Option<ImageRequest>,
    /// A Git source's, its raw objects too where `return_object`.
    pub git: Option<GitRequest>,
    /// An HTTP source's, resolved by fetching it.
    pub http: bool,
    /// The digest of an HTTP source's content followed by a suffix (ChecksumRequest).
    pub http_checksum: Option<ChecksumRequest>,
}

/// ChecksumRequest: a digest, by SHA-256, -384 or -512, of an HTTP source's content
/// followed by `suffix` (a signature's hash suffix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumRequest {
    pub algorithm: &'static str,
    pub suffix: Vec<u8>,
}

/// ResolveSourceGitRequest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitRequest {
    pub return_object: bool,
}

impl MetaRequest {
    /// Whether it asks for anything (sourceResolveRequest's nil request otherwise).
    fn asks(&self) -> bool {
        self.image.is_some() || self.git.is_some() || self.http || self.http_checksum.is_some()
    }
}

/// ResolveSourceImageRequest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRequest {
    pub no_config: bool,
    pub attestation_chain: bool,
    /// The predicate types of the attestations to read with the chain.
    pub resolve_attestations: Vec<String>,
}

/// What resolves a source's metadata when a policy asks for it.
pub trait Resolve {
    fn resolve(&self, source: &Source, request: &MetaRequest) -> Result<Meta, String>;
}

/// The policies' answer for one source (DecisionResponse), or what they ask to know
/// first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Deny(Vec<String>),
    Convert(Source),
    Resolve(MetaRequest),
}

/// What resolving a policy's unknowns comes to: a request for the source's metadata, or
/// its materials' resolved, to run the policy again.
#[derive(Debug)]
enum Step {
    Ask(MetaRequest),
    Retry,
}

/// A source the policies would not have, in BuildKit's words, and the messages the
/// policy that refused it gave (policysession.DenyMessagesError).
#[derive(Debug, Clone)]
pub struct Refused {
    pub error: String,
    pub messages: Vec<String>,
}

impl From<String> for Refused {
    fn from(error: String) -> Refused {
        Refused {
            error,
            messages: Vec::new(),
        }
    }
}

/// One policy (policy.Policy): its files, what the build says of itself, how much it
/// logs, and where its imports and `load_json` read.
struct Policy {
    files: Vec<(String, String)>,
    env: Env,
    level: LogLevel,
    fs: Fs,
    default_platform: Platform,
    skip_caps: bool,
}

/// The functions buildx gives its policies (policy/funcs.go), with their types.
fn functions() -> Vec<Function> {
    let s = || Type::String;
    let a = || Type::Any(Vec::new());
    let f = |args: Vec<Type>, result: Type| Type::Function {
        args,
        result: Some(Box::new(result)),
        variadic: None,
    };
    vec![
        Function {
            name: "load_json".into(),
            decl: f(vec![s()], a()),
        },
        Function {
            name: "verify_git_signature".into(),
            decl: f(vec![a(), s()], Type::Boolean),
        },
        Function {
            name: "verify_http_pgp_signature".into(),
            decl: f(vec![a(), s(), s()], Type::Boolean),
        },
        Function {
            name: "pin_image".into(),
            decl: f(vec![a(), s()], Type::Boolean),
        },
        Function {
            name: "artifact_attestation".into(),
            decl: f(vec![a(), s()], a()),
        },
        Function {
            name: "github_attestation".into(),
            decl: f(vec![a(), s()], a()),
        },
    ]
}

/// `data.docker.decision`.
fn query() -> Term {
    Term::reference(
        vec![
            Term::var("data", None),
            Term::string("docker", None),
            Term::string("decision", None),
        ],
        None,
    )
}

/// A ref of the input as rego.Unknowns parses one: `input.image.checksum`.
fn input_ref(path: &str) -> Term {
    let mut parts = Vec::new();
    for (i, seg) in path.split('.').enumerate() {
        let (name, index) = match seg.split_once('[') {
            Some((n, rest)) => (n, rest.strip_suffix(']').and_then(|x| x.parse::<i64>().ok())),
            None => (seg, None),
        };
        parts.push(if i == 0 {
            Term::var(name, None)
        } else {
            Term::string(name, None)
        });
        if let Some(n) = index {
            parts.push(Term::new(
                TermValue::Number(shards_rego::value::Number::from_i64(n)),
                None,
            ));
        }
    }
    Term::reference(parts, None)
}

/// What evaluating a policy once tells (a rego.New's Partial or Eval): its decision, or
/// for a partial one the unknown fields its decision reads; what print printed; the
/// image pins `pin_image` set; the fields the functions found missing
/// (runtimeUnknownInputRefs), and the digest `verify_http_pgp_signature` asked for.
struct Run {
    decision: Result<Option<Decision>, String>,
    unknown: Vec<String>,
    pins: Vec<String>,
    runtime: Vec<String>,
    checksum: Option<ChecksumRequest>,
}

/// The functions' state for one evaluation (policy `state`): the input they compare
/// their operands to, the digests `pin_image` pinned, the functions that could not
/// answer yet (`Unknowns`), and the digest a signature needs
/// (checksumNeededForSignature).
struct Funcs<'a> {
    input: &'a Input,
    input_value: Value,
    fs: &'a Fs,
    pins: Vec<String>,
    unknowns: Vec<&'static str>,
    checksum: Option<ChecksumRequest>,
    /// The build's thread, which says the lines and fetches the sources.
    ask: &'a mpsc::Sender<Ask>,
    trust: &'a signatures::Trust,
}

/// Go's %T of an OPA value.
fn ast_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "ast.Null",
        Value::Bool(_) => "ast.Boolean",
        Value::Number(_) => "ast.Number",
        Value::String(_) => "ast.String",
        Value::Array(_) => "*ast.Array",
        Value::Object(_) => "*ast.object",
        Value::Set(_) => "*ast.set",
    }
}

/// `time.Unix(secs, 0).String()` in this process's zone: the C library's where there is
/// one, UTC elsewhere (D103).
fn local_time(secs: i64) -> String {
    #[cfg(unix)]
    let zone = |s: i64| crate::cli::listing::local(s);
    #[cfg(not(unix))]
    let zone = shards_cmdline::format::utc;
    shards_cmdline::format::Clock { now: 0, zone: &zone }.string(i128::from(secs) * 1_000_000_000)
}

impl Funcs<'_> {
    /// Policy.log: a line for the policy's step, at `level`.
    fn say(&self, level: LogLevel, text: String) {
        let _ = self.ask.send(Ask::Log(level, text));
    }

    /// An HTTP source solved and read, as the build's thread fetches it.
    fn fetch(&self, name: String, url: String, accept: Option<&'static str>) -> Result<Vec<u8>, String> {
        let (reply, answer) = mpsc::channel();
        self.ask
            .send(Ask::Fetch {
                name,
                url,
                accept,
                reply,
            })
            .map_err(|_| "the build has ended".to_string())?;
        answer.recv().map_err(|_| "the build has ended".to_string())?
    }

    fn add_unknown(&mut self, name: &'static str) {
        if !self.unknowns.contains(&name) {
            self.unknowns.push(name);
        }
    }

    /// Policy.readFile: the file of the policy's FS, its first `limit` bytes.
    fn read_file(&self, path: &str, limit: usize) -> Result<Vec<u8>, HostError> {
        let mut data = self.fs.read(path).map_err(|e| {
            HostError::Undefined(match e {
                Missing::NotFound => format!(
                    "failed opening file {}: open {path}: file does not exist",
                    shards_cmdline::go::quote(path)
                ),
                Missing::Failed(m) => m,
            })
        })?;
        data.truncate(limit);
        Ok(data)
    }
}

impl Host for Funcs<'_> {
    /// Policy.Print: at Info, in order with what the functions say.
    fn print(&mut self, line: &str) -> bool {
        self.say(LogLevel::Info, line.to_string());
        true
    }

    fn call(&mut self, name: &str, args: &[Value]) -> Result<Option<Value>, HostError> {
        let undefined = |m: String| HostError::Undefined(m);
        match name {
            // builtinLoadJSONImpl: a JSON file of the policy's FS, its first 4 MiB.
            "load_json" => {
                let Some(Value::String(path)) = args.first() else {
                    return Err(undefined(format!(
                        "load_json: expected string path, got {}",
                        args.first().map_or("nil", Value::type_name)
                    )));
                };
                let mut data = self.fs.read(path).map_err(|e| match e {
                    Missing::NotFound => undefined(format!(
                        "failed opening file {}: open {path}: file does not exist",
                        shards_cmdline::go::quote(path)
                    )),
                    Missing::Failed(m) => undefined(m),
                })?;
                data.truncate(4 << 20);
                let text = String::from_utf8(data).map_err(|e| {
                    undefined(format!(
                        "load_json: invalid JSON in {}: {e}",
                        shards_cmdline::go::quote(path)
                    ))
                })?;
                shards_rego::value::from_json(&text).map(Some).map_err(|e| {
                    undefined(format!(
                        "load_json: invalid JSON in {}: {}",
                        shards_cmdline::go::quote(path),
                        e.0
                    ))
                })
            }
            // builtinPinImageImpl: the image pinned to a digest, unless it is that already.
            "pin_image" => {
                let Some(image) = &self.input.image else {
                    return Ok(Some(Value::Bool(false)));
                };
                let Some(arg) = args.first() else {
                    return Err(undefined("pin_image: expected object, got nil".into()));
                };
                if !matches!(arg, Value::Object(_)) {
                    return Err(undefined(format!(
                        "pin_image: expected object, got {}",
                        ast_type(arg)
                    )));
                }
                if self.input_value.get(&Value::string("image")) != Some(arg) {
                    return Err(undefined(
                        "pin_image: first argument is not the same as input image".into(),
                    ));
                }
                let Some(Value::String(d)) = args.get(1) else {
                    return Err(undefined("pin_image: expected string path".into()));
                };
                shards_image::reference::Digest::parse(d)
                    .map_err(|e| undefined(format!("pin_image: invalid digest: {e}")))?;
                if image.checksum != **d && !self.pins.iter().any(|p| **p == **d) {
                    self.pins.push(d.to_string());
                }
                Ok(Some(Value::Bool(true)))
            }
            // builtinVerifyGitSignatureImpl: the commit's or tag's signature, by the
            // keys of a file of the policy's FS; false where the source is not Git, and
            // unknown until its objects are fetched.
            "verify_git_signature" => {
                const NAME: &str = "verify_git_signature";
                let Some(git) = &self.input.git else {
                    return Ok(Some(Value::Bool(false)));
                };
                let Some(commit) = &git.commit else {
                    self.add_unknown(NAME);
                    return Ok(Some(Value::Bool(false)));
                };
                let arg = args.first().unwrap_or(&Value::Null);
                if !matches!(arg, Value::Object(_)) {
                    return Err(undefined(format!(
                        "{NAME}: expected object, got {}",
                        ast_type(arg)
                    )));
                }
                let git_value = self.input_value.get(&Value::string("git"));
                let field = |k: &str| git_value.and_then(|g| g.get(&Value::string(k)));
                let object = if field("commit") == Some(arg) {
                    &commit.object
                } else if let Some(tag) = git.tag.as_ref().filter(|_| field("tag") == Some(arg)) {
                    &tag.object
                } else {
                    return Err(undefined(format!("{NAME}: object is neither commit nor tag")));
                };
                let path = match args.get(1) {
                    Some(Value::String(p)) => p,
                    other => {
                        return Err(undefined(format!(
                            "{NAME}: expected string path, got {}",
                            other.map_or("<nil>", ast_type)
                        )));
                    }
                };
                let keys = self.read_file(path, 128 * 1024)?;
                let quote = |b: &[u8]| shards_dockerfile::go::quote(b);
                let formats = shards_gitsign::pgpsign::Formats {
                    quote: &quote,
                    time: &local_time,
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
                shards_gitsign::pgpsign::verify_git_signature(
                    &object.signature,
                    &object.signed_data,
                    &keys,
                    now,
                    &formats,
                )
                .map_err(|e| undefined(format!("{NAME}: verification failes: {e}")))?;
                Ok(Some(Value::Bool(true)))
            }
            // builtinVerifyHTTPPGPSignatureImpl: a detached signature of the download, by
            // the keys of a file, checked over the digest of the content and the
            // signature's hash suffix, which the source is fetched again for.
            "verify_http_pgp_signature" => {
                const NAME: &str = "verify_http_pgp_signature";
                let Some(http) = &self.input.http else {
                    return Ok(Some(Value::Bool(false)));
                };
                let arg = args.first().unwrap_or(&Value::Null);
                if !matches!(arg, Value::Object(_)) {
                    return Err(undefined(format!(
                        "{NAME}: expected object, got {}",
                        ast_type(arg)
                    )));
                }
                if self.input_value.get(&Value::string("http")) != Some(arg) {
                    return Err(undefined(format!(
                        "{NAME}: first argument is not the same as input http"
                    )));
                }
                let (sig_path, key_path) = match (args.get(1), args.get(2)) {
                    (Some(Value::String(s)), Some(Value::String(k))) => (s, k),
                    (Some(Value::String(_)), other) => {
                        return Err(undefined(format!(
                            "{NAME}: expected string pubkey path, got {}",
                            other.map_or("<nil>", ast_type)
                        )));
                    }
                    (other, _) => {
                        return Err(undefined(format!(
                            "{NAME}: expected string signature path, got {}",
                            other.map_or("<nil>", ast_type)
                        )));
                    }
                };
                let signature = self.read_file(sig_path, 512 * 1024)?;
                let keys = self.read_file(key_path, 512 * 1024)?;
                let (sig, _) = shards_gitsign::parse_armored_detached_signature(&signature)
                    .map_err(|e| undefined(format!("{NAME}: failed to parse detached signature: {e}")))?;
                let ring = shards_gitsign::pgpsign::read_all_armored_key_rings(&keys)
                    .map_err(|e| undefined(format!("{NAME}: failed to read armored keyring: {e}")))?;
                use shards_gitsign::signature::Hash;
                let algorithm = match sig.hash {
                    Some(Hash::Sha256) => "sha256",
                    Some(Hash::Sha384) => "sha384",
                    Some(Hash::Sha512) => "sha512",
                    _ => {
                        return Err(undefined(format!(
                            "{NAME}: unsupported signature hash: unsupported signature hash algorithm {}",
                            shards_gitsign::pgpsign::hash_name(sig.hash)
                        )));
                    }
                };
                let request = ChecksumRequest {
                    algorithm,
                    suffix: sig.hash_suffix.clone(),
                };
                let answered = http
                    .signature_checksum
                    .as_ref()
                    .filter(|(d, suffix)| !d.is_empty() && *suffix == request.suffix);
                let Some((digest, _)) = answered else {
                    self.checksum = Some(request);
                    self.add_unknown(NAME);
                    return Ok(Some(Value::Bool(false)));
                };
                shards_image::reference::Digest::parse(digest)
                    .map_err(|e| undefined(format!("{NAME}: invalid checksum digest: {e}")))?;
                let (got, hex) = digest.split_once(':').unwrap_or_default();
                if got != algorithm {
                    self.checksum = Some(request);
                    self.add_unknown(NAME);
                    return Ok(Some(Value::Bool(false)));
                }
                Ok(Some(Value::Bool(
                    shards_gitsign::pgpsign::verify_signature_with_digest(&sig, &ring, got, hex).is_ok(),
                )))
            }
            // builtinArtifactAttestationImpl and builtinGithubAttestationImpl: a SLSA
            // provenance bundle over the download, from a file of the policy's FS or
            // from GitHub's attestations of a repository, verified against Sigstore's
            // trusted root; undefined where none verifies, and unknown until the
            // download's digest is known.
            "artifact_attestation" | "github_attestation" => {
                let github = name == "github_attestation";
                let name: &'static str = if github {
                    "github_attestation"
                } else {
                    "artifact_attestation"
                };
                let Some(http) = &self.input.http else {
                    return Ok(None);
                };
                let arg = args.first().unwrap_or(&Value::Null);
                if !matches!(arg, Value::Object(_)) {
                    return Err(undefined(format!(
                        "{name}: expected object, got {}",
                        ast_type(arg)
                    )));
                }
                if self.input_value.get(&Value::string("http")) != Some(arg) {
                    return Err(undefined(format!(
                        "{name}: first argument is not the same as input http"
                    )));
                }
                let Some(Value::String(second)) = args.get(1) else {
                    return Err(undefined(format!(
                        "{name}: expected {}, got {}",
                        if github {
                            "repository name string"
                        } else {
                            "string path"
                        },
                        args.get(1).map_or("<nil>", ast_type)
                    )));
                };
                if http.checksum.is_empty() {
                    self.add_unknown(name);
                    return Ok(None);
                }
                shards_image::reference::Digest::parse(&http.checksum)
                    .map_err(|e| undefined(format!("{name}: invalid checksum: {e}")))?;
                let dgst = http.checksum.as_str();
                let trust = self.trust;
                let verify = |bundle: &[u8]| {
                    shards_sigstore::helpers::verify_artifact(
                        dgst,
                        bundle,
                        &|| trust.root(),
                        signatures::local_offset,
                        false,
                    )
                };
                if !github {
                    let bundle = self.read_file(second, 8 << 20)?;
                    trust
                        .verifier()
                        .map_err(|e| undefined(format!("{name}: getting policy verifier: {e}")))?;
                    return Ok(verify(&bundle)
                        .ok()
                        .map(|si| signatures::attestation_signature(&si).value()));
                }
                trust
                    .verifier()
                    .map_err(|e| undefined(format!("{name}: getting policy verifier: {e}")))?;
                // ast.String's %s: quoted.
                let repo = shards_cmdline::go::quote(second);
                let bundles = match github::bundles(second, dgst, &|n, u, a| self.fetch(n, u, a), &|line| {
                    self.say(LogLevel::Info, format!("{name}: {line}"));
                }) {
                    Ok(b) => b,
                    Err(e) => {
                        self.say(
                            LogLevel::Info,
                            format!("{name}: failed reading bundles for {repo}@{dgst}: {e}"),
                        );
                        return Ok(None);
                    }
                };
                if bundles.is_empty() {
                    self.say(
                        LogLevel::Info,
                        format!("{name}: no bundle found for {repo}@{dgst}"),
                    );
                    return Ok(None);
                }
                for bundle in &bundles {
                    match verify(bundle) {
                        Ok(si) => return Ok(Some(signatures::attestation_signature(&si).value())),
                        Err(e) => self.say(
                            LogLevel::Info,
                            format!("{name}: failed verifying bundle for {repo}@{dgst}: {}", e.0),
                        ),
                    }
                }
                Ok(None)
            }
            other => Err(HostError::Halt(format!(
                "{other} in a policy is not supported by shards yet"
            ))),
        }
    }
}

impl Policy {
    /// Parses its modules, and those they import from its FS (WithModuleLoader), and
    /// compiles them as rego.New does: OPA's own errors, as ast.Errors prints them.
    fn compile(&self) -> Result<Program, String> {
        let mut modules = BTreeMap::new();
        let mut errors = Vec::new();
        for (file, src) in std::iter::once((BUILTIN_MODULE.0, BUILTIN_MODULE.1))
            .chain(self.files.iter().map(|(f, s)| (f.as_str(), s.as_str())))
        {
            match shards_rego::parser::parse_module(file, src) {
                Ok(m) => {
                    modules.insert(file.to_string(), m);
                }
                Err(e) => errors.extend(e.errors()),
            }
        }
        if !errors.is_empty() {
            return Err(errors_text(&errors));
        }
        let fs = self.fs.clone();
        let loader: shards_rego::compile::Loader = Box::new(move |resolved| import_modules(&fs, resolved));
        let mut comp = Compiler::new(modules, functions(), true).with_loader(loader);
        comp.compile();
        if !comp.errors.is_empty() {
            return Err(errors_text(&comp.errors));
        }
        Ok(Program::new(
            &comp,
            functions().into_iter().map(|f| f.name).collect(),
        ))
    }

    /// One evaluation of `input`, compiled afresh as rego.New compiles: where `partial`
    /// and it leaves fields unknown, a partial one that says which of them its decision
    /// reads (collectUnknowns over the support modules); else the decision.
    fn run(&self, input: &Input, partial: bool, ask: &mpsc::Sender<Ask>, trust: &signatures::Trust) -> Run {
        let mut out = Run {
            decision: Ok(None),
            unknown: Vec::new(),
            pins: Vec::new(),
            runtime: Vec::new(),
            checksum: None,
        };
        let program = match self.compile() {
            Ok(p) => p,
            Err(e) => {
                out.decision = Err(e);
                return out;
            }
        };
        let value = input.json().value();
        let mut host = Funcs {
            input,
            input_value: value.clone(),
            fs: &self.fs,
            pins: Vec::new(),
            unknowns: Vec::new(),
            checksum: None,
            ask,
            trust,
        };
        let unknowns = input.unknown_refs();
        let mut m = Machine::new(&program, &mut host, context());
        if partial && !unknowns.is_empty() {
            let terms: Vec<Term> = unknowns.iter().map(|u| input_ref(u)).collect();
            match partial_query(&mut m, &query(), Some(value), &terms) {
                Ok(p) => out.unknown = collect_unknowns(&p.support, &unknowns),
                Err(e) => out.decision = Err(e.to_string()),
            }
            drop(m);
            out.runtime = runtime_refs(&host.unknowns);
            out.checksum = host.checksum;
            return out;
        }
        out.decision = eval_query(&mut m, &query(), Some(value))
            .map_err(|e| e.to_string())
            .and_then(decision_of)
            .map(Some);
        drop(m);
        let mut pins = host.pins;
        pins.sort();
        out.pins = pins;
        out.runtime = runtime_refs(&host.unknowns);
        out.checksum = host.checksum;
        out
    }
}

/// runtimeUnknownInputRefs: the input a function that could not answer needs.
fn runtime_refs(unknowns: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    if unknowns.contains(&"verify_git_signature") {
        out.push("git.commit".to_string());
    }
    if unknowns.contains(&"artifact_attestation") || unknowns.contains(&"github_attestation") {
        out.push("http.checksum".to_string());
    }
    out
}

/// A query's context: its time now, and randomness from the system.
fn context() -> shards_rego::funcs::Context {
    let time_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    shards_rego::funcs::Context {
        time_ns,
        fill: Some(|buf| aws_lc_rs::rand::fill(buf).map_err(|_| "reading random bytes failed".to_string())),
        ..Default::default()
    }
}

/// ast.Errors' text: `1 error occurred: …`, or their count and each on its own line.
fn errors_text(errors: &[shards_rego::parser::Error]) -> String {
    match errors {
        [e] => format!("1 error occurred: {e}"),
        es => {
            let mut out = format!("{} errors occurred:", es.len());
            for e in es {
                out.push('\n');
                out.push_str(&e.to_string());
            }
            out
        }
    }
}

/// buildx's module loader: each module a `data.` import names and the set lacks, read
/// from the policy's FS as the import's path, `/`-separated, with `.rego`, and placed
/// in the import's package.
fn import_modules(fs: &Fs, resolved: &BTreeMap<String, Module>) -> Result<BTreeMap<String, Module>, String> {
    let mut out = BTreeMap::new();
    for (k, m) in resolved {
        for imp in &m.imports {
            let pv = imp.path.to_string();
            let Some(pkg) = pv.strip_prefix("data.") else {
                continue;
            };
            let file = format!("{}.rego", pkg.replace('.', "/"));
            if resolved.contains_key(&file) || out.contains_key(&file) {
                continue;
            }
            let data = match fs.read(&file) {
                Ok(d) => d,
                Err(Missing::NotFound) => {
                    let place = fs.place(&file).ok().flatten();
                    return Err(match place {
                        None => format!("no policy FS defined for import {pv}"),
                        Some(_) => format!(
                            "import {pv} not found for module {k}: statat {file}: no such file or directory"
                        ),
                    });
                }
                Err(Missing::Failed(e)) => return Err(format!("import {pv} not found for module {k}: {e}")),
            };
            let src = String::from_utf8_lossy(&data);
            let mut module = shards_rego::parser::parse_module(&file, &src)
                .map_err(|e| format!("failed to parse imported policy file {file} for module {k}: {e}"))?;
            let mut path = vec![
                module
                    .package
                    .path
                    .first()
                    .cloned()
                    .unwrap_or_else(|| Term::var("data", None)),
            ];
            path.extend(pkg.split('.').map(|p| Term::string(p, None)));
            module.package.path = path;
            out.insert(file, module);
        }
    }
    Ok(out)
}

/// collectUnknowns: the input refs the support modules read, each trimmed to its field
/// (trimKey), kept where it is one of `allowed` or below one, in the order met.
fn collect_unknowns(support: &[Module], allowed: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    for m in support {
        walk_module_refs(m, &mut |r| {
            if r.first().and_then(Term::as_var) != Some("input") {
                return;
            }
            let k = trim_key(&Term::reference(r.to_vec(), None).to_string());
            if !k.is_empty() && !seen.contains(&k) {
                seen.push(k);
            }
        });
    }
    let valid: Vec<String> = allowed
        .iter()
        .map(|a| trim_key(a))
        .filter(|a| !a.is_empty())
        .collect();
    let mut out: Vec<String> = Vec::new();
    for k in seen {
        if let Some(m) = allowed_or_parent(&k, &valid)
            && !out.contains(&m)
        {
            out.push(m);
        }
    }
    out
}

/// matchAllowedOrParent: `key`, or its nearest parent on a component boundary, where
/// it is allowed.
fn allowed_or_parent(key: &str, allowed: &[String]) -> Option<String> {
    if allowed.iter().any(|a| a == key) {
        return Some(key.to_string());
    }
    for (i, c) in key.char_indices().rev() {
        if i > 0 && (c == '.' || c == '[') {
            let candidate = key.get(..i)?;
            if allowed.iter().any(|a| a == candidate) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// trimKey: an input ref without `input.`, cut to its first two components, but for a
/// provenance material's, kept whole.
fn trim_key(s: &str) -> String {
    let s = s.strip_prefix("input.").unwrap_or(s);
    if s.starts_with("image.provenance.materials[") {
        return s.to_string();
    }
    let mut components = 0;
    for (i, c) in s.char_indices() {
        if c == '.' || c == '[' {
            components += 1;
            if components == 2 {
                return s.get(..i).unwrap_or(s).to_string();
            }
        }
    }
    s.to_string()
}

/// ast.WalkRefs over a module's rules, in GenericVisitor's order.
fn walk_module_refs(m: &Module, f: &mut dyn FnMut(&[Term])) {
    let mut g = |t: &Term| -> bool {
        if let TermValue::Ref(r) = &t.value {
            f(r);
        }
        false
    };
    for rule in &m.rules {
        let mut r = Some(rule);
        while let Some(x) = r {
            let head = Term::reference(x.head.reference.clone(), None);
            shards_rego::compile::safety::walk_terms(&head, &mut g);
            for t in x.head.args.iter().chain(&x.head.key).chain(&x.head.value) {
                shards_rego::compile::safety::walk_terms(t, &mut g);
            }
            for e in &x.body {
                shards_rego::compile::safety::walk_terms_expr(e, &mut g);
            }
            r = x.else_.as_deref();
        }
    }
}

/// summarizeUnknownsForLog: each without `input.`, provenance and signatures as one, in
/// Go's `%+v` of a slice.
fn summarize(unknowns: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for u in unknowns {
        let mut u = u.strip_prefix("input.").unwrap_or(u).to_string();
        if u.starts_with("image.signatures") {
            u = "image.signatures".into();
        }
        if u.starts_with("image.provenance") {
            u = "image.provenance".into();
        }
        if u == "image" || out.contains(&u) {
            continue;
        }
        out.push(u);
    }
    format!("[{}]", out.join(" "))
}

/// AddUnknownsWithLogger: what of the source a request must ask for, to answer
/// `unknowns`.
fn request_for(unknowns: &[String], request: &mut MetaRequest) -> Result<(), String> {
    for u in unknowns {
        let u = u.as_str();
        if matches!(u, "image" | "git" | "http" | "local") {
            continue;
        }
        if u == "image.provenance"
            || u.starts_with("image.provenance.")
            || u == "image.hasProvenance"
            || u == "image.signatures"
        {
            let img = request.image.get_or_insert(ImageRequest {
                no_config: true,
                attestation_chain: false,
                resolve_attestations: Vec::new(),
            });
            img.attestation_chain = true;
            if u == "image.provenance" || u.starts_with("image.provenance.") {
                for t in [shards_sigstore::image::SLSA_V02, shards_sigstore::image::SLSA_V1] {
                    if !img.resolve_attestations.iter().any(|x| x == t) {
                        img.resolve_attestations.push(t.to_string());
                    }
                }
            }
            continue;
        }
        match u {
            "image.checksum" | "image.labels" | "image.user" | "image.volumes" | "image.workingDir"
            | "image.env" => {
                request.image.get_or_insert_with(ImageRequest::default).no_config = false;
            }
            // Resolved by BuildKit for the HTTP source itself.
            "http.checksum" => {}
            "git.ref" | "git.checksum" | "git.commitChecksum" | "git.isAnnotatedTag" | "git.isSHA256"
            | "git.tagName" | "git.branch" => {
                request.git.get_or_insert_with(GitRequest::default);
            }
            "git.commit" | "git.tag" => {
                request.git.get_or_insert_with(GitRequest::default).return_object = true
            }
            _ => return Err(format!("unhandled unknown property {u}")),
        }
        // hasHTTPUnknowns: any of an HTTP source's asks for it.
        if u.starts_with("http.") {
            request.http = true;
        }
    }
    Ok(())
}

/// A source's name in the log: its identifier, and its platform where it has one.
fn source_name(source: &Source, platform: Option<&Platform>) -> String {
    match platform {
        Some(p) => format!(
            "{} ({})",
            source.identifier,
            String::from_utf8_lossy(&platform::format(p))
        ),
        None => source.identifier.clone(),
    }
}

/// Where a policy's log lines go, the build's `loading policies` step, and what fetches
/// the HTTP sources its functions read (SourceResolver.ResolveState, then ReadFile).
pub trait Log {
    fn line(&self, text: &str);

    /// The content of `url`, asked for with `accept`, fetched as a step named `name`;
    /// the error as the read of the solved source words it.
    fn fetch(&self, name: &str, url: &str, accept: Option<&str>) -> Result<Vec<u8>, String>;
}

/// `policy eval --print`'s input of `source` for `platform`, `fields` resolved through
/// `resolver`: the JSON buildx prints, the fields that named nothing unknown, and what is
/// left unknown (eval.rs).
pub fn print_input(
    source: &Source,
    platform: &Platform,
    fields: &[String],
    resolver: &dyn Resolve,
) -> Result<(String, Vec<String>, Vec<String>), String> {
    let trust = signatures::Trust::default();
    let printed = eval::print_input(source, platform, fields, resolver, &trust)?;
    Ok((
        printed.input.json().indented(),
        printed.invalid,
        printed.unresolved,
    ))
}

/// What a policy's evaluation asks of the build's thread as it runs: a line for its
/// step, said as it is said, or an HTTP source fetched as a step of its own.
enum Ask {
    Log(LogLevel, String),
    Fetch {
        name: String,
        url: String,
        accept: Option<&'static str>,
        reply: mpsc::Sender<Result<Vec<u8>, String>>,
    },
}

/// The policies a build heeds, and how much each logs.
pub struct Policies {
    list: Vec<Policy>,
    /// The files, for the step's name.
    pub names: Vec<String>,
    /// The messages of the last denial, which the build prints as `Policy: …`.
    pub denied: RefCell<Vec<String>>,
    /// The build's signature verifier (SignatureVerifier).
    trust: signatures::Trust,
}

/// configureSourcePolicy's setup of one build's policies: the Dockerfile's own and the
/// flags', each file read.
pub struct Setup<'a> {
    pub default: Opt,
    pub configs: &'a [PolicyConfig],
    pub env: Env,
    pub cwd: PathBuf,
    pub default_platform: Platform,
    pub debug: bool,
    /// Whether buildx's default policy may come first (a build's; not `policy eval`'s).
    pub default_policy: bool,
}

impl Policies {
    /// The build's policies, or none: with_config, then resolvePolicyOpts.
    pub fn configure(setup: Setup<'_>) -> Result<Option<Policies>, String> {
        let mut opts = with_config(setup.default, setup.configs)?;
        // The default policy first, where it is enabled and no policy is disabled, its
        // caps never asked.
        if setup.default_policy && default_policy_enabled() && !setup.configs.iter().any(|c| c.disabled) {
            opts.insert(
                0,
                Opt {
                    files: vec![FileSpec {
                        filename: DEFAULT_POLICY.0.into(),
                        optional: false,
                        data: Some(DEFAULT_POLICY.1.as_bytes().to_vec()),
                    }],
                    skip_caps: true,
                    ..Opt::default()
                },
            );
        }
        let level = if setup.debug {
            LogLevel::Debug
        } else {
            LogLevel::Info
        };
        let mut list = Vec::new();
        for opt in opts {
            let fs = Fs {
                context_dir: opt.context_dir.clone(),
                cwd: setup.cwd.clone(),
            };
            let mut files = Vec::new();
            for f in &opt.files {
                let data = match &f.data {
                    Some(d) => d.clone(),
                    None => match fs.read(&f.filename) {
                        Ok(d) => d,
                        Err(Missing::NotFound) if f.optional => continue,
                        Err(Missing::NotFound) => {
                            return Err(format!("policy file {} not found", f.filename));
                        }
                        Err(Missing::Failed(e)) => return Err(e),
                    },
                };
                files.push((f.filename.clone(), String::from_utf8_lossy(&data).into_owned()));
            }
            if files.is_empty() {
                continue;
            }
            list.push(Policy {
                files,
                env: setup.env.clone(),
                level: opt.log_level.unwrap_or(level),
                fs,
                default_platform: setup.default_platform.clone(),
                skip_caps: opt.skip_caps,
            });
        }
        if list.is_empty() {
            return Ok(None);
        }
        Ok(Some(Policies {
            names: list
                .iter()
                .flat_map(|p| p.files.iter().map(|(f, _)| f.clone()))
                .collect(),
            list,
            denied: RefCell::new(Vec::new()),
            trust: signatures::Trust::default(),
        }))
    }

    /// Each policy asked for its caps, as the build begins (applyPolicyCaps).
    pub fn check_caps(&self, log: &dyn Log) -> Result<(), String> {
        for p in self.list.iter().filter(|p| !p.skip_caps) {
            self.caps(p, log)
                .map_err(|e| format!("failed to evaluate policy caps: {e}"))?;
        }
        Ok(())
    }

    fn say(&self, p: &Policy, level: LogLevel, log: &dyn Log, text: &str) {
        if level <= p.level {
            log.line(text);
        }
    }

    /// CheckCaps: the policy asked as the build starts, with no source; the caps it asks
    /// for refused, shards' builder having no network proxy for them yet.
    fn caps(&self, p: &Policy, log: &dyn Log) -> Result<(), String> {
        let input = Input {
            env: Env {
                caps_request: true,
                ..p.env.clone()
            },
            ..Input::default()
        };
        self.say(
            p,
            LogLevel::Debug,
            log,
            &format!("policy input: {}", input.json().indented()),
        );
        let run = self.run(p, &input, false, log)?;
        let decision = run.decision?.ok_or("policy returned zero result")?;
        if decision.caps.iter().any(|(_, on)| *on) {
            return Err("network proxy requested by policy is not supported by shards yet".into());
        }
        Ok(())
    }

    /// The policies' verdict on `source`, loaded for `platform` (policyEvaluator.evaluate):
    /// `None` where every policy allows it as it is; the source a policy converted it to,
    /// itself allowed; or why it may not be loaded. Each question of its metadata is put
    /// to `resolver`, and every policy asked again.
    pub fn evaluate(
        &self,
        source: &Source,
        platform: Option<&Platform>,
        resolver: &dyn Resolve,
        log: &dyn Log,
    ) -> Result<Option<Source>, Refused> {
        let mut source = source.clone();
        let mut converted = false;
        let mut meta = Meta::default();
        for _ in 0..MAX_RESOLVES {
            match self.answer(&source, platform, &meta, resolver, log)? {
                Answer::Resolve(request) => {
                    meta = resolver
                        .resolve(&source, &request)
                        .map_err(|e| format!("error resolving source metadata from policy request: {e}"))?;
                }
                Answer::Convert(to) => {
                    source = to;
                    converted = true;
                    meta = Meta::default();
                }
                Answer::Allow => return Ok(converted.then_some(source)),
                Answer::Deny(messages) => {
                    self.denied.borrow_mut().clone_from(&messages);
                    return Err(Refused {
                        error: format!(
                            "source {} not allowed by policy: action DENY",
                            shards_cmdline::go::quote(&source.identifier)
                        ),
                        messages,
                    });
                }
            }
        }
        Err("too many policy requests".to_string().into())
    }

    /// One answer of the policies for `source` with what is known of it (`meta`), as
    /// `policy eval` asks for one at a time (CheckPolicy): the request for more, or the
    /// decision.
    pub fn check_once(
        &self,
        source: &Source,
        platform: Option<&Platform>,
        meta: &Meta,
        resolver: &dyn Resolve,
        log: &dyn Log,
    ) -> Result<Answer, String> {
        self.answer(source, platform, meta, resolver, log)
    }

    /// MultiPolicyCallback: each policy in turn, the first to deny, convert, or ask for
    /// more answering for all.
    fn answer(
        &self,
        source: &Source,
        platform: Option<&Platform>,
        meta: &Meta,
        resolver: &dyn Resolve,
        log: &dyn Log,
    ) -> Result<Answer, String> {
        for p in &self.list {
            match self.check(p, source, platform, meta, resolver, log)? {
                Answer::Allow => {}
                other => return Ok(other),
            }
        }
        Ok(Answer::Allow)
    }

    /// resolveUnknowns: for a signature's digest, the request for it and for all of
    /// `unk`; else ResolveInputUnknowns: the request for what `unk` needs of the source
    /// itself, or its materials' resolved through `resolver` (`Retry`: run again).
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        &self,
        p: &Policy,
        input: &mut Input,
        source: &Source,
        platform: &Platform,
        name: &str,
        unk: &[String],
        checksum: Option<ChecksumRequest>,
        resolver: &dyn Resolve,
        log: &dyn Log,
    ) -> Result<Option<Step>, String> {
        let mut say = |level: LogLevel, text: &str| self.say(p, level, log, text);
        let request = match checksum {
            Some(c) => {
                let mut request = MetaRequest {
                    platform: Some(platform.clone()),
                    ..MetaRequest::default()
                };
                let unk: Vec<String> = unk
                    .iter()
                    .map(|u| u.strip_prefix("input.").unwrap_or(u).to_string())
                    .filter(|u| !u.is_empty())
                    .collect();
                provenance::add_unknowns(&unk, &mut request, &mut say)?;
                request.http = true;
                request.http_checksum = Some(c);
                request
            }
            None => {
                match provenance::resolve_input_unknowns(
                    input,
                    source,
                    unk,
                    Some(platform),
                    Some(platform),
                    Some(resolver),
                    Some(&self.trust),
                    &mut say,
                )? {
                    (_, Some(request)) => request,
                    (true, None) => return Ok(Some(Step::Retry)),
                    (false, None) => return Ok(None),
                }
            }
        };
        say(
            LogLevel::Info,
            &format!(
                "policy decision for source {name}: resolve missing fields {}",
                summarize(unk)
            ),
        );
        Ok(Some(Step::Ask(request)))
    }

    /// One evaluation of `p` on its thread, this one saying its lines and fetching its
    /// sources meanwhile.
    fn run(&self, p: &Policy, input: &Input, partial: bool, log: &dyn Log) -> Result<Run, String> {
        let trust = &self.trust;
        with_stack_serving(|ask| p.run(input, partial, ask, trust), &|a| match a {
            Ask::Log(level, line) => self.say(p, level, log, &line),
            Ask::Fetch {
                name,
                url,
                accept,
                reply,
            } => {
                let _ = reply.send(log.fetch(&name, &url, accept));
            }
        })
    }

    /// CheckPolicy: one policy's answer for `source`, its provenance's materials resolved
    /// through `resolver` as the policy asks of them.
    fn check(
        &self,
        p: &Policy,
        source: &Source,
        platform: Option<&Platform>,
        meta: &Meta,
        resolver: &dyn Resolve,
        log: &dyn Log,
    ) -> Result<Answer, String> {
        let normalized = platform.map(platform::normalize);
        let wanted = normalized.as_ref().unwrap_or(&p.default_platform);
        let mut input = provenance::source_to_input(
            source,
            meta,
            Some(wanted),
            Some(&self.trust),
            &mut |level, text| self.say(p, level, log, text),
        )
        .map_err(|e| format!("failed to build policy input: {e}"))?;
        let name = source_name(source, normalized.as_ref());
        self.say(
            p,
            LogLevel::Info,
            log,
            &format!("checking policy for source {name}"),
        );
        for _ in 0..MAX_RESOLVES {
            let mut run_input = input.clone();
            provenance::apply_env(&mut run_input, &p.env, 0);
            if let Some(answer) =
                self.decide(p, &mut input, &run_input, source, wanted, &name, resolver, log)?
            {
                return Ok(answer);
            }
        }
        Err("maximum attempts reached for resolving policy metadata".into())
    }

    /// One pass of CheckPolicy's loop over `run_input`: the answer, or `None` where what
    /// the policy asked of the materials was resolved into `input`, to run again.
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        p: &Policy,
        input: &mut Input,
        run_input: &Input,
        source: &Source,
        wanted: &Platform,
        name: &str,
        resolver: &dyn Resolve,
        log: &dyn Log,
    ) -> Result<Option<Answer>, String> {
        self.say(
            p,
            LogLevel::Debug,
            log,
            &format!("policy input: {}", run_input.json().indented()),
        );
        let unknowns = input.unknown_refs();
        if !unknowns.is_empty() {
            self.say(
                p,
                LogLevel::Debug,
                log,
                &format!("unknowns for policy evaluation: {}", summarize(&unknowns)),
            );
            let run = self.run(p, run_input, true, log)?;
            run.decision?;
            let mut unk: Vec<String> = run
                .unknown
                .iter()
                .map(|u| u.strip_prefix("input.").unwrap_or(u).to_string())
                .collect();
            unk.extend(run.runtime);
            match self.resolve(p, input, source, wanted, name, &unk, run.checksum, resolver, log)? {
                Some(Step::Ask(request)) => return Ok(Some(Answer::Resolve(request))),
                Some(Step::Retry) => return Ok(None),
                None => {}
            }
        }
        let run = self.run(p, run_input, false, log)?;
        let decision = run.decision?;
        // What the functions could not answer is fetched, and the policy run again.
        match self.resolve(
            p,
            input,
            source,
            wanted,
            name,
            &run.runtime,
            run.checksum,
            resolver,
            log,
        )? {
            Some(Step::Ask(request)) => return Ok(Some(Answer::Resolve(request))),
            Some(Step::Retry) => return Ok(None),
            None => {}
        }
        let decision = decision.ok_or("policy returned zero result")?;
        self.say(
            p,
            LogLevel::Debug,
            log,
            &format!(
                "policy response: &{{Allow:{} DenyMessages:[{}] Caps:map[]}}",
                decision.allow.map_or("<nil>".to_string(), |a| a.to_string()),
                decision.deny.join(" ")
            ),
        );
        if decision.allow == Some(true) {
            match run.pins.as_slice() {
                [] => {}
                [pin] => {
                    let to = pinned(source, pin)?;
                    self.say(
                        p,
                        LogLevel::Info,
                        log,
                        &format!("policy decision for source {name}: convert to {}", to.identifier),
                    );
                    return Ok(Some(Answer::Convert(to)));
                }
                pins => {
                    return Err(format!(
                        "multiple image pins set to {name}: map[{}]",
                        pins.join(":{} ") + ":{}"
                    ));
                }
            }
            self.say(
                p,
                LogLevel::Info,
                log,
                &format!("policy decision for source {name}: ALLOW"),
            );
            return Ok(Some(Answer::Allow));
        }
        self.say(
            p,
            LogLevel::Info,
            log,
            &format!("policy decision for source {name}: DENY"),
        );
        for m in &decision.deny {
            self.say(p, LogLevel::Info, log, &format!(" - {m}"));
        }
        Ok(Some(Answer::Deny(decision.deny)))
    }
}

/// addPinToImage: the image source pinned to `digest`.
fn pinned(source: &Source, digest: &str) -> Result<Source, String> {
    let Some(id) = source.identifier.strip_prefix("docker-image://") else {
        return Err(format!(
            "cannot pin non-image source: {}",
            shards_cmdline::go::quote(&source.identifier)
        ));
    };
    let mut r = shards_image::reference::Reference::parse_normalized(id).map_err(|e| {
        format!(
            "failed parsing image reference {}: {e}",
            shards_cmdline::go::quote(id)
        )
    })?;
    r.digest = Some(shards_image::reference::Digest::parse(digest).map_err(|e| {
        format!(
            "failed adding digest to image reference {}: {e}",
            shards_cmdline::go::quote(id)
        )
    })?);
    Ok(Source {
        identifier: format!("docker-image://{r}"),
        attrs: source.attrs.clone(),
    })
}

/// A decision as buildx reads one (parsePolicyDecision).
struct Decision {
    allow: Option<bool>,
    deny: Vec<String>,
    caps: Vec<(String, bool)>,
}

/// policyDecisionFromResult: the first result's value, an object.
fn decision_of(results: Vec<Value>) -> Result<Decision, String> {
    let Some(v) = results.into_iter().next() else {
        return Err("policy returned zero result".into());
    };
    let Value::Object(o) = &v else {
        return Err(format!(
            "unexpected policy return type: {} data.docker.decision",
            go_type(&v)
        ));
    };
    let mut d = Decision {
        allow: None,
        deny: Vec::new(),
        caps: Vec::new(),
    };
    if let Some(a) = o.get(&Value::string("allow")) {
        match a {
            Value::Bool(b) => d.allow = Some(*b),
            other => {
                return Err(format!(
                    "invalid allowed property type {}, expecting bool",
                    go_type(other)
                ));
            }
        }
    }
    if let Some(Value::Array(ms)) = o.get(&Value::string("deny_msg")) {
        d.deny = ms.iter().filter_map(|m| m.as_str().map(str::to_string)).collect();
    }
    if let Some(Value::Set(ms)) = o.get(&Value::string("deny_msg")) {
        d.deny = ms.iter().filter_map(|m| m.as_str().map(str::to_string)).collect();
    }
    if let Some(c) = o.get(&Value::string("caps")) {
        let Value::Object(cs) = c else {
            return Err(format!(
                "invalid caps property type {}, expecting object",
                go_type(c)
            ));
        };
        for (k, v) in cs.iter() {
            let k = k.as_str().unwrap_or_default();
            if k != "exec.proxy" {
                return Err(format!("unknown policy cap {}", shards_cmdline::go::quote(k)));
            }
            let Value::Bool(b) = v else {
                return Err(format!(
                    "invalid caps.{k} property type {}, expecting bool",
                    go_type(v)
                ));
            };
            d.caps.push((k.to_string(), *b));
        }
    }
    Ok(d)
}

/// The Go type rego.Eval gives a value (ast.JSON): `%T` of it.
fn go_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "<nil>",
        Value::Bool(_) => "bool",
        Value::Number(_) => "json.Number",
        Value::String(_) => "string",
        Value::Array(_) | Value::Set(_) => "[]interface {}",
        Value::Object(_) => "map[string]interface {}",
    }
}

/// Rego's parser, compiler and evaluator recurse as deep as the documents and rules they
/// walk, which Go's growable stacks allow: they run on a thread with room for the
/// deepest a policy may be, the calling thread answering what the evaluation asks until
/// it ends.
fn with_stack_serving<T: Send>(
    f: impl FnOnce(&mpsc::Sender<Ask>) -> T + Send,
    serve: &dyn Fn(Ask),
) -> Result<T, String> {
    #[cfg(test)]
    let size = tests::PROBE.with(std::cell::Cell::get).unwrap_or(STACK);
    #[cfg(not(test))]
    let size = STACK;
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        let evaluation = std::thread::Builder::new()
            .name("policy".into())
            .stack_size(size)
            .spawn_scoped(s, move || f(&tx))
            .map_err(|e| format!("starting the policy's evaluation: {e}"))?;
        // The evaluation's sender goes when it ends, which ends this.
        for ask in rx {
            serve(ask);
        }
        evaluation
            .join()
            .map_err(|_| "the policy's evaluation failed".to_string())
    })
}

/// A thread of `size` for the stack measurement, as [`with_stack_serving`] makes one.
#[cfg(test)]
fn with_stack_of<T: Send>(size: usize, f: impl FnOnce() -> T + Send) -> Result<T, String> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("policy".into())
            .stack_size(size)
            .spawn_scoped(s, f)
            .map_err(|e| format!("starting the policy's evaluation: {e}"))?
            .join()
            .map_err(|_| "the policy's evaluation failed".to_string())
    })
}

/// The policy thread's stack: room for the deepest policy OPA parses (its parser's
/// 100000 levels) and the deepest JSON `load_json` reads (Go's 10000), each parsed,
/// compiled, evaluated and dropped. Measured (M126): 123 MiB on aarch64-apple-darwin and
/// 120 on x86_64-apple-darwin; `the_deepest_policy_runs_on_the_policy_thread` holds every
/// target to it.
const STACK: usize = 123 << 20;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    thread_local! {
        /// A stack size to measure with in place of [`STACK`].
        pub(super) static PROBE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    struct Lines(RefCell<Vec<String>>);

    impl Log for Lines {
        fn line(&self, text: &str) {
            self.0.borrow_mut().push(text.to_string());
        }

        fn fetch(&self, name: &str, url: &str, _: Option<&str>) -> Result<Vec<u8>, String> {
            self.0.borrow_mut().push(format!("FETCH {name} {url}"));
            Err("invalid response status 404".into())
        }
    }

    /// A resolver with nothing to tell.
    struct NoMeta;

    impl Resolve for NoMeta {
        fn resolve(&self, source: &Source, _: &MetaRequest) -> Result<Meta, String> {
            Err(format!("no metadata for {}", source.identifier))
        }
    }

    fn cfg(files: &[&str]) -> PolicyConfig {
        PolicyConfig {
            files: files.iter().map(|f| f.to_string()).collect(),
            ..PolicyConfig::default()
        }
    }

    fn names(opts: &[Opt]) -> Vec<Vec<String>> {
        opts.iter()
            .map(|o| o.files.iter().map(|f| f.filename.clone()).collect())
            .collect()
    }

    // build/build_test.go's TestWithPolicyConfig cases, as withPolicyConfig answers them.
    #[test]
    fn policy_flags_combine_as_buildx_combines_them() {
        let default = Opt {
            files: vec![FileSpec {
                filename: "Dockerfile.rego".into(),
                optional: true,
                data: None,
            }],
            ..Opt::default()
        };
        assert_eq!(
            names(&with_config(default.clone(), &[]).unwrap()),
            [["Dockerfile.rego"]]
        );
        assert!(with_config(Opt::default(), &[]).unwrap().is_empty());
        assert_eq!(
            names(&with_config(default.clone(), &[cfg(&["a.rego"])]).unwrap()),
            [vec!["Dockerfile.rego"], vec!["a.rego"]]
        );
        let reset = PolicyConfig {
            reset: true,
            ..cfg(&["b.rego"])
        };
        assert_eq!(
            names(&with_config(default.clone(), &[cfg(&["a.rego"]), reset]).unwrap()),
            [["b.rego"]]
        );
        // strict and log-level alone set the policy before them...
        let strict = PolicyConfig {
            strict: Some(true),
            log_level: Some(LogLevel::Debug),
            ..PolicyConfig::default()
        };
        let out = with_config(default.clone(), std::slice::from_ref(&strict)).unwrap();
        assert!(out[0].strict);
        assert_eq!(out[0].log_level, Some(LogLevel::Debug));
        // ...or, with none before, the next.
        let out = with_config(Opt::default(), &[strict, cfg(&["a.rego"])]).unwrap();
        assert!(out[0].strict);
        assert_eq!(out[0].log_level, Some(LogLevel::Debug));
        let off = PolicyConfig {
            disabled: true,
            ..PolicyConfig::default()
        };
        assert!(
            with_config(default.clone(), std::slice::from_ref(&off))
                .unwrap()
                .is_empty()
        );
        for bad in [
            vec![off.clone(), cfg(&["a.rego"])],
            vec![PolicyConfig {
                reset: true,
                ..off.clone()
            }],
        ] {
            assert_eq!(
                with_config(default.clone(), &bad).unwrap_err(),
                "disabled policy cannot be combined with other policy flags"
            );
        }
    }

    // validate.go's trimKey and collectUnknowns, as buildx's tests pin them.
    #[test]
    fn unknowns_are_collected_as_buildx_collects_them() {
        assert_eq!(trim_key("input.image.labels[\"a\"]"), "image.labels");
        assert_eq!(trim_key("input.image"), "image");
        assert_eq!(
            trim_key("input.image.provenance.materials[0].image.checksum"),
            "image.provenance.materials[0].image.checksum"
        );
        let allowed = ["image.checksum".to_string(), "image.provenance".to_string()];
        assert_eq!(
            allowed_or_parent("image.provenance.x", &allowed).as_deref(),
            Some("image.provenance")
        );
        assert_eq!(allowed_or_parent("image.tag", &allowed), None);
        assert_eq!(
            summarize(&[
                "input.image.checksum".into(),
                "input.image.signatures[0]".into(),
                "input.image".into(),
                "input.image.checksum".into()
            ]),
            "[image.checksum image.signatures]"
        );
    }

    fn policy_in(dir: &Path, src: &str) -> Policies {
        std::fs::create_dir_all(dir).unwrap();
        Policies::configure(Setup {
            default: Opt {
                files: vec![FileSpec {
                    filename: "Dockerfile.rego".into(),
                    optional: true,
                    data: Some(src.as_bytes().to_vec()),
                }],
                context_dir: Some(dir.to_path_buf()),
                ..Opt::default()
            },
            configs: &[],
            env: Env::default(),
            cwd: dir.to_path_buf(),
            default_platform: Platform::new("linux", "arm64"),
            debug: false,
            default_policy: true,
        })
        .unwrap()
        .unwrap()
    }

    /// An image whose provenance names a material, and the material's digest.
    struct Provenanced(RefCell<Vec<(String, MetaRequest)>>);

    impl Resolve for Provenanced {
        fn resolve(&self, source: &Source, request: &MetaRequest) -> Result<Meta, String> {
            self.0
                .borrow_mut()
                .push((source.identifier.clone(), request.clone()));
            let digest = |c: char| format!("sha256:{}", c.to_string().repeat(64));
            let statement = format!(
                r#"{{"predicateType":"{}","predicate":{{"buildDefinition":{{"buildType":"t","resolvedDependencies":[{{"uri":"pkg:docker/golang@1.25?platform=linux%2Famd64"}}]}}}}}}"#,
                shards_sigstore::image::SLSA_V1
            );
            let mut desc = shards_sigstore::image::Descriptor::default();
            desc.annotations.insert(
                "in-toto.io/predicate-type".into(),
                shards_sigstore::image::SLSA_V1.into(),
            );
            let chain = AttestationChain {
                attestation_manifest: digest('d'),
                blobs: [(digest('1'), (desc, statement.into_bytes()))].into(),
                ..AttestationChain::default()
            };
            let image = match source.identifier.as_str() {
                "docker-image://docker.io/library/alpine:3.20" => ImageMeta {
                    digest: digest('a'),
                    config: None,
                    attestation_chain: Some(chain),
                },
                "docker-image://docker.io/library/golang:1.25" => ImageMeta {
                    digest: digest('b'),
                    config: None,
                    attestation_chain: None,
                },
                other => return Err(format!("no metadata for {other}")),
            };
            Ok(Meta {
                image: Some(image),
                ..Meta::default()
            })
        }
    }

    /// CheckPolicy's loop: the image's provenance asked of BuildKit, then its material's
    /// checksum resolved by the policy itself, for the material's own platform, and the
    /// policy run again.
    #[test]
    fn a_policy_reads_provenance_materials() {
        let dir = scratch("materials");
        let p = policy_in(
            &dir,
            &format!(
                "package docker\n\ndefault allow := false\n\nallow if {{\n\tinput.local\n}}\n\nallow if {{\n\tinput.image.provenance.materials[0].image.checksum == \"sha256:{}\"\n}}\n\ndecision := {{\"allow\": allow}}\n",
                "b".repeat(64)
            ),
        );
        let log = Lines(RefCell::new(Vec::new()));
        let resolver = Provenanced(RefCell::new(Vec::new()));
        let alpine = Source::new("docker-image://docker.io/library/alpine:3.20");
        let verdict = p.evaluate(&alpine, None, &resolver, &log);
        assert!(matches!(verdict, Ok(None)), "{verdict:?} {:?}", log.0.borrow());
        let calls = resolver.0.borrow();
        let asked: Vec<(&str, Option<&ImageRequest>, Option<&Platform>)> = calls
            .iter()
            .map(|(s, r)| (s.as_str(), r.image.as_ref(), r.platform.as_ref()))
            .collect();
        let amd64 = Platform::new("linux", "amd64");
        let arm64 = Platform::new("linux", "arm64");
        assert_eq!(
            asked,
            [
                (
                    "docker-image://docker.io/library/alpine:3.20",
                    Some(&ImageRequest {
                        no_config: true,
                        attestation_chain: true,
                        resolve_attestations: vec![
                            shards_sigstore::image::SLSA_V02.into(),
                            shards_sigstore::image::SLSA_V1.into()
                        ],
                    }),
                    Some(&arm64)
                ),
                (
                    "docker-image://docker.io/library/golang:1.25",
                    Some(&ImageRequest::default()),
                    Some(&amd64)
                ),
            ]
        );
        let lines = log.0.borrow();
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("checking policy for source"))
                .count(),
            2,
            "{lines:?}"
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some("policy decision for source docker-image://docker.io/library/alpine:3.20: ALLOW")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("shards-policy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The deepest a policy goes, run on its thread: a JSON document of 10000 levels,
    /// Go's limit, read by `load_json`, compared and dropped; and a policy nested as deep
    /// as OPA's parser takes, parsed, compiled and evaluated. `SHARDS_STACK_PROBE` names
    /// another size to measure with.
    #[test]
    fn the_deepest_policy_runs_on_the_policy_thread() {
        let probe = std::env::var("SHARDS_STACK_PROBE")
            .ok()
            .and_then(|v| v.parse().ok());
        PROBE.with(|p| p.set(probe));
        let size = probe.unwrap_or(STACK);
        let dir = scratch("deep");
        std::fs::create_dir_all(&dir).unwrap();
        let depth = 10000;
        std::fs::write(
            dir.join("deep.json"),
            format!("{}{}", "[".repeat(depth), "]".repeat(depth)),
        )
        .unwrap();
        let p = policy_in(
            &dir,
            "package docker\n\nx := load_json(\"deep.json\")\n\ndecision := {\"allow\": x == x}\n",
        );
        let log = Lines(RefCell::new(Vec::new()));
        let local = Source::new("local://context");
        assert_eq!(
            p.answer(&local, None, &Meta::default(), &NoMeta, &log).unwrap(),
            Answer::Allow,
            "{:?}",
            log.0.borrow()
        );
        // The deepest array the parser takes, found on a thread of the size measured.
        let nest = |n: usize| format!("{}1{}", "[".repeat(n), "]".repeat(n));
        let parses = |n: usize| {
            shards_rego::parser::parse_module("p.rego", &format!("package p\nx := {}\n", nest(n))).is_ok()
        };
        let deepest = with_stack_of(size, || {
            let (mut lo, mut hi) = (1usize, 100_000usize);
            while lo < hi {
                let mid = lo + (hi - lo).div_ceil(2);
                if parses(mid) {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            lo
        })
        .unwrap();
        assert!(deepest > 1000, "{deepest}");
        let p = policy_in(
            &dir,
            &format!(
                "package docker\n\nx := {}\n\ndecision := {{\"allow\": x == x}}\n",
                nest(deepest)
            ),
        );
        assert_eq!(
            p.answer(&local, None, &Meta::default(), &NoMeta, &log).unwrap(),
            Answer::Allow,
            "{:?}",
            log.0.borrow()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
