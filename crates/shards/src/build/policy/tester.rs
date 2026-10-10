//! `policy test`, as buildx v0.37.1 runs a policy's tests (policy/tester.go
//! RunPolicyTests, commands/policy/test.go runTest): the `*_test.rego` modules of a
//! directory or a file, compiled beside the policy (`<filename>.rego`) and buildx's own
//! module; each `test_` rule asked of them, with the input its `with input as` gives,
//! decoded into buildx's Input types; an image input first resolved as the policy alone
//! would have it; then what the policy decides of that input, and what of the input the
//! policy reads that it lacks. Every file is read as `os.DirFS(".")` reads the working
//! directory, here `root`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use shards_cmdline::buildflags::{LogLevel, os_error};
use shards_dockerfile::platform::{self, Platform};
use shards_rego::ast::{Module, Term, TermValue};
use shards_rego::compile::Compiler;
use shards_rego::eval::{Machine, Program, eval_query};
use shards_rego::value::Value;

use super::input::{self, Input, Json};
use super::testinput::{self, GoInput};
use super::{
    Answer, BUILTIN_MODULE, Fs, Funcs, Log, Meta, MetaRequest, Policies, Policy, Resolve, Source, context,
    errors_text, functions, provenance, runtime_refs, signatures, trim_key, walk_module_refs,
};

/// Modules by their file's name.
type Modules = BTreeMap<String, Module>;

/// TestOptions: the tests to run, by a substring of their names; the policy's file, less
/// `.rego`; and the directory every name is read in (the working directory).
pub struct TestOptions {
    pub run: String,
    pub filename: String,
    pub root: PathBuf,
}

/// TestOptionsProvider: the builder's platform, for an image input that names none, and
/// what resolves a source's metadata as the policy asks for it. The verifier the policy
/// resolves with is the home's (SignatureVerifier).
pub trait TestProvider {
    fn platform(&self) -> Result<Platform, String>;
    fn resolve(&self, source: &Source, req: &MetaRequest) -> Result<Meta, String>;
}

/// TestResult.
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub allow: Option<bool>,
    pub deny_messages: Vec<String>,
    /// The input the test ran with, as JSON writes it; none where it has none.
    pub input: Option<Json>,
    /// What the policy decided of that input (Decision), as JSON writes it.
    pub decision: Option<Json>,
    /// The fields of the input the policy reads and the input lacks, without `input.`.
    pub missing_input: Vec<String>,
    /// The sources' metadata that would answer them: `git`, `image`.
    pub metadata_needed: Vec<String>,
}

/// TestSummary.
pub struct TestSummary {
    pub results: Vec<TestResult>,
    pub failed: usize,
}

// ---- os.DirFS ----

/// dirFS.join: `name` in `root`, where it is a valid name (fs.ValidPath, and on Windows
/// filepathlite.Localize's further refusals).
fn local(root: &Path, name: &str) -> Option<PathBuf> {
    if name == "." {
        return Some(root.to_path_buf());
    }
    if name.contains('\0') || name.split('/').any(|e| e.is_empty() || e == "." || e == "..") {
        return None;
    }
    if cfg!(windows) && (name.contains('\\') || name.contains(':')) {
        return None;
    }
    Some(root.join(name))
}

/// dirFS.Stat: whether `name` is a directory, following links.
fn stat(root: &Path, name: &str) -> Result<bool, String> {
    let path = local(root, name).ok_or_else(|| format!("stat {name}: invalid argument"))?;
    std::fs::metadata(path)
        .map(|m| m.is_dir())
        .map_err(|e| format!("stat {name}: {}", os_error(&e)))
}

/// fs.ReadFile over dirFS.ReadFile: a directory opens, and fails as it is read.
fn read(root: &Path, name: &str) -> Result<Vec<u8>, String> {
    let path = local(root, name).ok_or_else(|| format!("readfile {name}: invalid argument"))?;
    std::fs::read(path).map_err(|e| {
        let op = if e.kind() == std::io::ErrorKind::IsADirectory {
            "read"
        } else {
            "open"
        };
        format!("{op} {name}: {}", os_error(&e))
    })
}

/// fs.ReadDir: the entries by name, each a directory or not as its type says (a link is
/// not).
fn read_dir(root: &Path, name: &str) -> Result<Vec<(String, bool)>, String> {
    let path = local(root, name).ok_or_else(|| format!("readdir {name}: invalid argument"))?;
    let fail = |e: std::io::Error| format!("open {name}: {}", os_error(&e));
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let dir = entry.file_type().is_ok_and(|t| t.is_dir());
        out.push((entry.file_name().to_string_lossy().into_owned(), dir));
    }
    out.sort();
    Ok(out)
}

/// Policy.readFile over os.DirFS: what the functions read, a policy test's.
pub(super) fn read_file(root: &Path, name: &str) -> Result<Vec<u8>, String> {
    let q = shards_cmdline::go::quote(name);
    let path =
        local(root, name).ok_or_else(|| format!("failed opening file {q}: open {name}: invalid argument"))?;
    std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::IsADirectory => format!("failed reading {q}: read {name}: is a directory"),
        _ => format!("failed opening file {q}: open {name}: {}", os_error(&e)),
    })
}

/// A module loader over os.DirFS(root): each module a `data.` import names and the set
/// lacks the file of, read and placed in the import's package; the tester's
/// (`packages`) also passes over an import a module of the set has the package of. The
/// modules in name order, where Go's map gives them in any: of two imports that fail,
/// the first by its module's name is the one told.
pub(super) fn import_modules(root: &Path, resolved: &Modules, packages: bool) -> Result<Modules, String> {
    let has_package = |pkg: &str| {
        resolved.values().any(|m| {
            let p = package(m);
            p == pkg || p.strip_prefix("data.") == Some(pkg)
        })
    };
    let mut out = BTreeMap::new();
    for (k, m) in resolved {
        for imp in &m.imports {
            let pv = imp.path.to_string();
            let Some(pkg) = pv.strip_prefix("data.") else {
                continue;
            };
            if packages && has_package(pkg) {
                continue;
            }
            let file = format!("{}.rego", pkg.replace('.', "/"));
            if resolved.contains_key(&file) {
                continue;
            }
            stat(root, &file).map_err(|e| format!("import {pv} not found for module {k}: {e}"))?;
            let data = read(root, &file)
                .map_err(|e| format!("failed to read imported policy file {file} for module {k}: {e}"))?;
            let mut module = parse(&file, &data)
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

// ---- the modules ----

/// ast.ParseModuleWithOpts, its errors as ast.Errors prints them.
fn parse(file: &str, src: &[u8]) -> Result<Module, String> {
    shards_rego::parser::parse_module(file, &String::from_utf8_lossy(src))
        .map_err(|e| errors_text(&e.errors()))
}

/// A package's path as Rego prints it: `data.docker`.
fn package(m: &Module) -> String {
    Term::reference(m.package.path.clone(), None).to_string()
}

/// LoadTestModules: the `*_test.rego` files of a directory, or the one file named, each
/// parsed, by name.
fn load_test_modules(root: &Path, path: &str) -> Result<Modules, String> {
    let path = path.replace(std::path::MAIN_SEPARATOR, "/");
    let path = match path.strip_suffix('/').unwrap_or(&path) {
        "" => ".",
        p => p,
    };
    let dir = stat(root, path).map_err(|e| format!("stat {path}: {e}"))?;
    let mut files = Vec::new();
    if dir {
        for (name, dir) in read_dir(root, path).map_err(|e| format!("read dir {path}: {e}"))? {
            if dir || !name.ends_with("_test.rego") {
                continue;
            }
            // filepath.Join: cleaned.
            let joined = format!("{path}/{name}");
            files
                .push(String::from_utf8_lossy(&shards_dockerfile::go::clean(joined.as_bytes())).into_owned());
        }
    } else {
        if !path.ends_with("_test.rego") {
            return Err(format!("test file must have _test.rego suffix: {path}"));
        }
        files.push(path.to_string());
    }
    if files.is_empty() {
        return Err("no policy tests found".into());
    }
    files.sort();
    let mut modules = BTreeMap::new();
    for file in files {
        let data = read(root, &file).map_err(|e| format!("read policy test module {file}: {e}"))?;
        let module = parse(&file, &data).map_err(|e| format!("parse policy test module {file}: {e}"))?;
        modules.insert(file, module);
    }
    Ok(modules)
}

/// loadPolicyModules: `<filename>.rego` and buildx's own module, and the policy's file
/// as the policy is given it.
fn load_policy_modules(root: &Path, filename: &str) -> Result<(Modules, (String, String)), String> {
    if filename.is_empty() {
        return Err("policy filename is required".into());
    }
    let file = format!("{filename}.rego");
    let data = read(root, &file).map_err(|e| format!("read policy module {file}: {e}"))?;
    let module = parse(&file, &data).map_err(|e| format!("parse policy module {file}: {e}"))?;
    let key = file.replace(std::path::MAIN_SEPARATOR, "/");
    let mut modules = BTreeMap::new();
    modules.insert(key.clone(), module);
    if !modules.contains_key(BUILTIN_MODULE.0) {
        let builtin = parse(BUILTIN_MODULE.0, BUILTIN_MODULE.1.as_bytes())
            .map_err(|e| format!("parse builtin policy module {}: {e}", BUILTIN_MODULE.0))?;
        modules.insert(BUILTIN_MODULE.0.to_string(), builtin);
    }
    Ok((modules, (key, String::from_utf8_lossy(&data).into_owned())))
}

/// A test: its rule's name and its package's path.
struct TestDef {
    name: String,
    package: String,
}

/// findPolicyTests: the rules named `test_…` that take no arguments and are true or
/// false, by name. A name two packages define is the later module's by its file's name,
/// where Go's map makes it either.
fn find_tests(modules: &Modules) -> Vec<TestDef> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for m in modules.values() {
        let pkg = package(m);
        for rule in &m.rules {
            let boolean = rule
                .head
                .value
                .as_ref()
                .is_none_or(|v| matches!(v.value, TermValue::Bool(_)));
            if !rule.head.args.is_empty() || !boolean {
                continue;
            }
            let name = rule.head.name.as_deref().unwrap_or_default();
            if name.starts_with("test_") {
                seen.insert(name.to_string(), pkg.clone());
            }
        }
    }
    seen.into_iter()
        .map(|(name, package)| TestDef { name, package })
        .collect()
}

/// lookupTestInput: the term a test's rules give `input` itself as (`with input as`),
/// the same one in each, decoded into buildx's Input.
fn test_input(modules: &Modules, t: &TestDef) -> Result<Option<GoInput>, String> {
    let mut term: Option<&Term> = None;
    for m in modules.values().filter(|m| package(m) == t.package) {
        for rule in m
            .rules
            .iter()
            .filter(|r| r.head.name.as_deref() == Some(t.name.as_str()))
        {
            for w in rule.body.iter().flat_map(|e| &e.with) {
                let is_input = matches!(&w.target.value, TermValue::Ref(r)
                    if r.len() == 1 && r.first().and_then(Term::as_var) == Some("input"));
                if !is_input {
                    continue;
                }
                if let Some(prev) = term
                    && !prev.equal(&w.value)
                {
                    return Err(format!("multiple input overrides for {}", t.name));
                }
                term = Some(&w.value);
            }
        }
    }
    match term {
        None => Ok(None),
        Some(term) => testinput::decode(&term.to_string())
            .map(Some)
            .map_err(|e| format!("failed to decode test input for {}: {e}", t.name)),
    }
}

// ---- running ----

/// The input a test runs with: as JSON writes it, and as the functions read it.
struct Effective {
    json: Json,
    typed: Input,
}

/// What of a decoded input the functions read: which sources it names and their
/// digests. A decoded commit or tag has no Git object behind it: buildx's
/// verify_git_signature dereferences the object it lacks, and panics, where shards finds
/// it unsigned.
fn shadow(g: &GoInput) -> Input {
    let object = || super::gitobject::GitObject {
        tag: false,
        headers: BTreeMap::new(),
        message: String::new(),
        signature: Vec::new(),
        signed_data: Vec::new(),
        raw: Vec::new(),
    };
    Input {
        env: g.env.to_env(),
        local: g.local.as_ref().map(|l| l.name.clone()),
        image: g.image.as_ref().map(|i| input::Image {
            checksum: i.checksum.clone(),
            ..input::Image::default()
        }),
        http: g.http.as_ref().map(|h| input::Http {
            checksum: h.checksum.clone(),
            ..input::Http::default()
        }),
        git: g.git.as_ref().map(|x| input::Git {
            commit: x.commit.as_ref().map(|_| input::CommitInput {
                commit: super::gitobject::Commit::default(),
                signature: shards_gitsign::Summary::None,
                object: object(),
            }),
            tag: x.tag.as_ref().map(|_| input::TagInput {
                tag: super::gitobject::Tag::default(),
                signature: shards_gitsign::Summary::None,
                object: object(),
            }),
            ..input::Git::default()
        }),
        unknowns: Vec::new(),
    }
}

/// What one evaluation of a test's query gave: its first result, if any, and the
/// functions that could not answer.
struct Asked {
    value: Result<Option<Value>, String>,
    runtime: Vec<String>,
}

/// newPolicyRego(...).Eval: `query` over the compiled modules, with `input` where the
/// test has one, and the functions of a policy with no logger, verifier or source
/// resolver.
fn ask(program: &Program, query: &Term, input: Option<&Effective>, fs: &Fs) -> Asked {
    // No build to say lines to or fetch through: what would go there is dropped.
    let (tx, _rx) = mpsc::channel();
    let empty = Input::default();
    let value = input.map(|i| i.json.value());
    let mut host = Funcs {
        input: input.map_or(&empty, |i| &i.typed),
        input_value: value.clone().unwrap_or(Value::Null),
        fs,
        pins: Vec::new(),
        unknowns: Vec::new(),
        checksum: None,
        ask: &tx,
        trust: None,
        resolver: false,
    };
    let mut m = Machine::new(program, &mut host, context());
    let value = eval_query(&mut m, query, value)
        .map(|v| v.into_iter().next())
        .map_err(|e| e.to_string());
    drop(m);
    Asked {
        value,
        runtime: runtime_refs(&host.unknowns),
    }
}

/// decodeDecision: the decision as JSON reads it into a map: `allow` where it is a
/// boolean, `deny_msg` a string or the strings of an array, `caps` the booleans of an
/// object; written back as buildx's Decision.
fn decision(v: &Value) -> Option<(Json, Option<bool>, Vec<String>)> {
    let Value::Object(o) = v else {
        return None;
    };
    let allow = match o.get(&Value::string("allow")) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    };
    let deny: Vec<String> = match o.get(&Value::string("deny_msg")) {
        Some(Value::String(s)) => vec![s.to_string()],
        Some(Value::Array(a)) => a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect(),
        Some(Value::Set(a)) => a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    let mut caps = BTreeMap::new();
    if let Some(Value::Object(c)) = o.get(&Value::string("caps")) {
        for (k, v) in c.iter() {
            if let (Some(k), Value::Bool(b)) = (k.as_str(), v) {
                caps.insert(k.to_string(), *b);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(a) = allow {
        out.push(("allow".to_string(), Json::Bool(a)));
    }
    if !deny.is_empty() {
        out.push((
            "deny_msg".to_string(),
            Json::Arr(deny.iter().map(|m| Json::Str(m.clone())).collect()),
        ));
    }
    if !caps.is_empty() {
        out.push((
            "caps".to_string(),
            Json::Obj(caps.into_iter().map(|(k, b)| (k, Json::Bool(b))).collect()),
        ));
    }
    Some((Json::Obj(out), allow, deny))
}

/// inputHasPath: each component a member of the object before it.
fn has_path(input: Option<&Json>, key: &str) -> bool {
    let Some(mut cur) = input else {
        return false;
    };
    for part in key.split('.') {
        let Json::Obj(members) = cur else {
            return false;
        };
        match members.iter().find(|(k, _)| k == part) {
            Some((_, v)) => cur = v,
            None => return false,
        }
    }
    true
}

/// missingInputRefs: each input ref the modules read (collectUnknowns, trimmed to its
/// field), and each the functions asked for, that the input does not have; none where
/// there are no modules. Repeats are the caller's to drop.
fn missing_refs(mods: &[&Module], input: Option<&Json>, extra: &[&[String]]) -> Vec<String> {
    if mods.is_empty() {
        return Vec::new();
    }
    let mut refs: Vec<String> = Vec::new();
    for m in mods {
        walk_module_refs(m, &mut |r| {
            if r.first().and_then(Term::as_var) == Some("input") {
                refs.push(trim_key(&Term::reference(r.to_vec(), None).to_string()));
            }
        });
    }
    refs.extend(extra.iter().flat_map(|e| e.iter().cloned()));
    refs.into_iter()
        .filter(|k| !k.is_empty() && !has_path(input, k))
        .collect()
}

/// summarizeMetadataRequests: the sources' metadata the missing fields would need.
fn metadata_needed(missing: &[String]) -> Vec<String> {
    let mut req = MetaRequest::default();
    if provenance::add_unknowns(missing, &mut req, &mut |_, _| {}).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if req.image.is_some() {
        out.push("image".to_string());
    }
    if req.git.is_some() {
        out.push("git".to_string());
    }
    out.sort();
    out
}

// ---- resolving an image input ----

/// A policy with no logger, and no source resolver to fetch through.
struct Quiet;

impl Log for Quiet {
    fn line(&self, _: &str) {}

    fn fetch(&self, _: &str, _: &str, _: Option<&str>) -> Result<Vec<u8>, String> {
        Err("source resolver is not configured".into())
    }
}

/// No source resolver for the materials: the policy's `resolver` is false.
struct NoResolver;

impl Resolve for NoResolver {
    fn resolve(&self, _: &Source, _: &MetaRequest) -> Result<Meta, String> {
        Err("material metadata resolution requires source resolver".into())
    }
}

/// sourceFromInput: an image input's reference, its tag added where it has no `:`.
fn source_of(input: &GoInput) -> Option<Source> {
    let img = input.image.as_ref()?;
    let mut r = [&img.reference, &img.full_repo, &img.repo]
        .into_iter()
        .find(|r| !r.is_empty())?
        .clone();
    if !img.tag.is_empty() && !r.contains(':') {
        r = format!("{r}:{}", img.tag);
    }
    Some(Source::new(format!("docker-image://{r}")))
}

/// containerd's defaults for a specifier of one part: GOOS, GOARCH.
fn go_runtime() -> Platform {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        "x86" => "386",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        a => a,
    };
    Platform::new(shards_sigstore::platforms::host_os(), arch)
}

/// platforms.Parse, then Normalize: its OS, architecture and variant.
fn parse_platform(spec: &[u8]) -> Result<Platform, Vec<u8>> {
    let p = platform::normalize(&platform::parse(spec, &go_runtime())?);
    Ok(Platform {
        os: p.os,
        architecture: p.architecture,
        variant: p.variant,
        ..Platform::default()
    })
}

/// inputPlatform: an image input's platform, parsed; else its OS, architecture and
/// variant as they are.
fn platform_of(input: &GoInput) -> Result<Option<Platform>, String> {
    let Some(img) = &input.image else {
        return Ok(None);
    };
    if !img.platform.is_empty() {
        return parse_platform(img.platform.as_bytes()).map(Some).map_err(|e| {
            format!(
                "invalid platform {}: {}",
                img.platform,
                String::from_utf8_lossy(&e)
            )
        });
    }
    if [&img.os, &img.arch, &img.variant].iter().all(|f| f.is_empty()) {
        return Ok(None);
    }
    Ok(Some(Platform {
        os: img.os.clone().into_bytes(),
        architecture: img.arch.clone().into_bytes(),
        variant: img.variant.clone().into_bytes(),
        ..Platform::default()
    }))
}

/// platformFromReq: the platform CheckPolicy is asked with, as `os/arch[/variant]`.
fn request_platform(p: &Platform) -> Result<Platform, String> {
    let mut spec = [p.os.as_slice(), b"/", &p.architecture].concat();
    if !p.variant.is_empty() {
        spec.push(b'/');
        spec.extend_from_slice(&p.variant);
    }
    parse_platform(&spec).map_err(|e| format!("failed to parse platform: {}", String::from_utf8_lossy(&e)))
}

/// What resolves a test's image input: the policy alone (its file, no tests), as a policy
/// with no logger or source resolver, but the home's verifier.
struct Resolution<'a> {
    file: &'a (String, String),
    fs: &'a Fs,
    provider: &'a dyn TestProvider,
    /// The verifier, kept for the run.
    checker: Policies,
}

impl Resolution<'_> {
    /// resolveTestInput: the input the policy, asking its questions of the provider, has
    /// for the image `input` names, once it asks nothing more and has what it reads of an
    /// image or a Git source, or none can answer it; five rounds at most. Its Env the
    /// test's, where that says anything.
    fn resolve(&self, modules: &[&Module], input: &GoInput) -> Result<Option<Effective>, String> {
        let Some(source) = source_of(input) else {
            return Ok(None);
        };
        let platform = match platform_of(input)? {
            Some(p) => p,
            None => self.provider.platform()?,
        };
        let env = if input.has_env() {
            input.env.to_env()
        } else {
            input::Env::default()
        };
        let policy = Policy {
            files: vec![self.file.clone()],
            env: env.clone(),
            level: LogLevel::Info,
            fs: self.fs.clone(),
            default_platform: platform.clone(),
            skip_caps: false,
            resolver: false,
        };
        let asked = request_platform(&platform)?;
        let mut meta = Meta::default();
        for _ in 0..5 {
            match self
                .checker
                .check(&policy, &source, Some(&asked), &meta, &NoResolver, &Quiet)?
            {
                Answer::Resolve(req) => meta = self.provider.resolve(&source, &req)?,
                _ => {
                    let mut inp = input::of_source(
                        &source,
                        &meta,
                        Some(&platform),
                        Some(&self.checker.trust),
                        &mut |_, _| {},
                    )?;
                    // resolveTestInput sets it, and mergeInputOverrides again.
                    if input.has_env() {
                        inp.env = env.clone();
                    }
                    let json = inp.json();
                    let resolvable: Vec<String> = missing_refs(modules, Some(&json), &[])
                        .into_iter()
                        .filter(|m| m.starts_with("image.") || m.starts_with("git."))
                        .collect();
                    let mut req = MetaRequest::default();
                    if !resolvable.is_empty()
                        && provenance::add_unknowns(&resolvable, &mut req, &mut |_, _| {}).is_ok()
                        && (req.image.is_some() || req.git.is_some())
                    {
                        meta = self.provider.resolve(&source, &req)?;
                        continue;
                    }
                    return Ok(Some(Effective { json, typed: inp }));
                }
            }
        }
        Err("maximum attempts reached for resolving policy metadata".into())
    }
}

/// What the tests ask of the thread that runs them: the builder's platform, or a source's
/// metadata.
enum Need {
    Platform(mpsc::Sender<Result<Platform, String>>),
    Resolve(Box<(Source, MetaRequest)>, mpsc::Sender<Result<Meta, String>>),
}

/// The provider, asked from the tests' own thread.
struct Asker(mpsc::Sender<Need>);

impl Asker {
    fn ask<T>(&self, need: impl FnOnce(mpsc::Sender<Result<T, String>>) -> Need) -> Result<T, String> {
        let (reply, answer) = mpsc::channel();
        let ended = || "policy tests have ended".to_string();
        self.0.send(need(reply)).map_err(|_| ended())?;
        answer.recv().map_err(|_| ended())?
    }
}

impl TestProvider for Asker {
    fn platform(&self) -> Result<Platform, String> {
        self.ask(Need::Platform)
    }

    fn resolve(&self, source: &Source, req: &MetaRequest) -> Result<Meta, String> {
        self.ask(|reply| Need::Resolve(Box::new((source.clone(), req.clone())), reply))
    }
}

/// RunPolicyTests: the tests under `path` (a directory of `*_test.rego` files, or one),
/// each run against the policy `opts.filename` names. `provider` resolves an image
/// input's metadata (TestOptions.Provider); without one, every input is as written. They
/// run on a thread with a build's policy stack, as deep as a build's policies may go; the
/// provider is asked on this one.
pub fn run_policy_tests(
    path: &str,
    opts: &TestOptions,
    provider: Option<&dyn TestProvider>,
) -> Result<TestSummary, String> {
    let (tx, needs) = mpsc::channel();
    let asker = provider.map(|_| Asker(tx));
    std::thread::scope(|s| {
        let tests = std::thread::Builder::new()
            .name("policy".into())
            .stack_size(super::STACK)
            .spawn_scoped(s, move || {
                run(path, opts, asker.as_ref().map(|a| a as &dyn TestProvider))
            })
            .map_err(|e| format!("starting the policy's tests: {e}"))?;
        if let Some(p) = provider {
            // Until the tests end, and their asker with them.
            for need in needs {
                match need {
                    Need::Platform(reply) => {
                        let _ = reply.send(p.platform());
                    }
                    Need::Resolve(asked, reply) => {
                        let _ = reply.send(p.resolve(&asked.0, &asked.1));
                    }
                }
            }
        }
        tests
            .join()
            .map_err(|_| "the policy's tests ended abnormally".to_string())?
    })
}

/// RunPolicyTests, on the tests' thread.
fn run(path: &str, opts: &TestOptions, provider: Option<&dyn TestProvider>) -> Result<TestSummary, String> {
    let root = opts.root.as_path();
    let (policy_modules, policy_file) = load_policy_modules(root, &opts.filename)?;
    let test_modules = load_test_modules(root, path)?;
    let mut modules = policy_modules.clone();
    modules.extend(test_modules.clone());
    let loader_root = opts.root.clone();
    let loader: shards_rego::compile::Loader =
        Box::new(move |resolved| import_modules(&loader_root, resolved, true));
    let mut comp = Compiler::new(modules, functions(), false).with_loader(loader);
    comp.compile();
    if !comp.errors.is_empty() {
        return Err(format!("compile: {}", errors_text(&comp.errors)));
    }
    let program = Program::new(&comp, functions().into_iter().map(|f| f.name).collect());
    let mut tests = find_tests(&test_modules);
    if !opts.run.is_empty() {
        tests.retain(|t| t.name.contains(&opts.run));
    }
    if tests.is_empty() {
        return Err("no tests found".into());
    }
    let fs = Fs {
        context_dir: None,
        remote: false,
        cwd: opts.root.clone(),
        dir: Some(opts.root.clone()),
    };
    let resolution = provider.map(|provider| Resolution {
        file: &policy_file,
        fs: &fs,
        provider,
        checker: Policies {
            list: Vec::new(),
            names: Vec::new(),
            denied: RefCell::new(Vec::new()),
            trust: signatures::Trust::default(),
        },
    });
    let mut summary = TestSummary {
        results: Vec::new(),
        failed: 0,
    };
    for t in &tests {
        let package_modules: Vec<&Module> = policy_modules
            .values()
            .filter(|m| package(m) == t.package)
            .collect();
        let decoded = test_input(&test_modules, t)?;
        let resolved = match (&resolution, &decoded) {
            (Some(r), Some(g)) => r.resolve(&package_modules, g)?,
            _ => None,
        };
        // The test's own input, where none is resolved: as rego.Input writes it, which
        // fails the run for a time JSON cannot write.
        let effective = match (resolved, &decoded) {
            (Some(r), _) => Some(r),
            (None, Some(g)) => Some(Effective {
                json: g.json()?,
                typed: shadow(g),
            }),
            (None, None) => None,
        };
        let query = |name: &str| {
            let mut r = test_modules
                .values()
                .find(|m| package(m) == t.package)
                .map(|m| m.package.path.clone())
                .unwrap_or_default();
            r.push(Term::string(name, None));
            Term::reference(r, None)
        };
        let test = ask(&program, &query(&t.name), effective.as_ref(), &fs);
        let passed = matches!(test.value?, Some(Value::Bool(true)));
        let decided = ask(&program, &query("decision"), effective.as_ref(), &fs);
        let (decision_json, allow, deny) = match decided.value.ok().flatten().as_ref().and_then(decision) {
            Some((j, a, d)) => (Some(j), a, d),
            None => (None, None, Vec::new()),
        };
        let mut missing = missing_refs(
            &package_modules,
            effective.as_ref().map(|e| &e.json),
            &[&test.runtime, &decided.runtime],
        );
        missing.sort();
        missing.dedup();
        let metadata = metadata_needed(&missing);
        summary.failed += usize::from(!passed);
        summary.results.push(TestResult {
            name: t.name.clone(),
            passed,
            allow,
            deny_messages: deny,
            input: effective.map(|e| e.json),
            decision: decision_json,
            missing_input: missing,
            metadata_needed: metadata,
        });
    }
    Ok(summary)
}

/// runTest's report: each test's line, and for a failed one its input, its decision,
/// what it lacks and what would answer it; the status, 1 where a test failed
/// (cobrautil.ExitCodeError(1)).
pub fn report(summary: &TestSummary, out: &mut dyn std::io::Write) -> u8 {
    for r in &summary.results {
        let status = if r.passed { "PASS" } else { "FAIL" };
        let allow = r.allow.map_or("n/a".to_string(), |a| a.to_string());
        let _ = if r.deny_messages.is_empty() {
            writeln!(out, "{}: {status} (allow={allow})", r.name)
        } else {
            writeln!(
                out,
                "{}: {status} (allow={allow}, deny_msg={})",
                r.name,
                r.deny_messages.join("; ")
            )
        };
        if r.passed {
            continue;
        }
        let _ = match &r.input {
            Some(i) => writeln!(out, "input:\n{}", i.indented()),
            None => writeln!(out, "input: <nil>"),
        };
        let _ = match &r.decision {
            Some(d) => writeln!(out, "decision:\n{}", d.indented()),
            None => writeln!(out, "decision: <nil>"),
        };
        if !r.missing_input.is_empty() {
            let keys: Vec<String> = r.missing_input.iter().map(|k| format!("input.{k}")).collect();
            let _ = writeln!(out, "missing_input: {}", keys.join(", "));
        }
        if !r.metadata_needed.is_empty() {
            let _ = writeln!(out, "metadata_resolve: {}", r.metadata_needed.join(", "));
        }
    }
    u8::from(summary.failed > 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use sha2::Digest as _;

    use super::*;

    const CONFIG: &str = r#"{"created":"2024-01-02T03:04:05Z","config":{"Labels":{"org.opencontainers.image.source":"https://github.com/x/y"},"User":"app","Env":["PATH=/bin"],"WorkingDir":"/w","Volumes":{"/data":{}}}}"#;

    /// The oracle's provider: the builder linux/amd64; every image's digest that of its
    /// identifier, and one config where one is asked for.
    struct Fake;

    impl TestProvider for Fake {
        fn platform(&self) -> Result<Platform, String> {
            Ok(Platform::new("linux", "amd64"))
        }

        fn resolve(&self, source: &Source, req: &MetaRequest) -> Result<Meta, String> {
            let mut meta = Meta::default();
            if source.identifier.starts_with("docker-image://") {
                let digest = sha2::Sha256::digest(source.identifier.as_bytes());
                let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                meta.image = Some(super::super::ImageMeta {
                    digest: format!("sha256:{hex}"),
                    config: req
                        .image
                        .as_ref()
                        .filter(|i| !i.no_config)
                        .map(|_| CONFIG.as_bytes().to_vec()),
                    attestation_chain: None,
                });
            }
            Ok(meta)
        }
    }

    /// Where buildx panics, what shards does instead: the case, and its report.
    const DEVIATIONS: &[(&str, &str)] = &[(
        "verify git commit",
        "test_a: FAIL (allow=false)\ninput:\n{\n  \"git\": {\n    \"commit\": {\n      \"tree\": \"t\"\n    }\n  }\n}\ndecision:\n{\n  \"allow\": false\n}\n",
    )];

    /// Every tree of scripts/policy/tester_oracle_test.go, run as buildx v0.37.1 runs it:
    /// what it prints, the error it ends with, and its status.
    #[test]
    fn policy_tests_run_as_buildx_runs_them() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../testdata/policy/tester.json")).unwrap();
        let base = std::env::temp_dir().join(format!("shards-policy-tester-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut failures = Vec::new();
        for (i, c) in cases.as_array().unwrap().iter().enumerate() {
            let dir = base.join(i.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            for (name, data) in c["files"].as_object().unwrap() {
                let p = dir.join(name);
                if name.ends_with('/') {
                    std::fs::create_dir_all(&p).unwrap();
                    continue;
                }
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, data.as_str().unwrap()).unwrap();
            }
            let opts = TestOptions {
                run: c["run"].as_str().unwrap().to_string(),
                filename: c["filename"].as_str().unwrap().to_string(),
                root: dir.clone(),
            };
            let (stdout, err, status) =
                match run_policy_tests(c["path"].as_str().unwrap(), &opts, Some(&Fake)) {
                    Ok(summary) => {
                        let mut out = Vec::new();
                        let status = report(&summary, &mut out);
                        (String::from_utf8(out).unwrap(), String::new(), i64::from(status))
                    }
                    Err(e) => (String::new(), e, 1),
                };
            let name = c["name"].as_str().unwrap();
            if let Some(panic) = c["panic"].as_str().filter(|p| !p.is_empty()) {
                let (_, ours) = DEVIATIONS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .unwrap_or_else(|| panic!("{name}: buildx panics ({panic}), and no deviation says why"));
                assert_eq!((stdout.as_str(), err.as_str(), status), (*ours, "", 1), "{name}");
                continue;
            }
            let want = (c["err"].as_str().unwrap(), c["stdout"].as_str().unwrap());
            // An error ending in the host's own words (a file not found, a directory read)
            // is buildx's on the host the oracle ran on, Linux. On Windows Go's words are
            // Windows' (os.Stat's GetFileAttributesEx, FormatMessage's text), which the
            // oracle does not record: there the error short of its last path error alone
            // is compared, as tests/buildx.rs compares one.
            let windows_words = cfg!(windows)
                && ["no such file or directory", "is a directory"]
                    .iter()
                    .any(|t| want.0.ends_with(t));
            let head = |s: &str| s.rsplitn(3, ": ").nth(2).map(str::to_string);
            let err_ok = if windows_words {
                head(want.0).is_some() && head(&err) == head(want.0)
            } else {
                err == want.0
            };
            if !err_ok || stdout != want.1 || status != c["status"].as_i64().unwrap() {
                failures.push(format!(
                    "{name}\n  got:  {status} {stdout:?} {err:?}\n  want: {} {:?} {:?}",
                    c["status"], want.1, want.0
                ));
            }
        }
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            failures.is_empty(),
            "{} cases differ:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
