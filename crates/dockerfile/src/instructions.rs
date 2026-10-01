//! Parsed lines into typed instructions and build stages, as BuildKit's
//! `frontend/dockerfile/instructions` makes them (dockerfile/1.27.1): each instruction's
//! flags (`BFlags`), arguments and errors, `RUN`'s mounts, network, security and devices,
//! and the build checks this stage runs (StageNameCasing, FromAsCasing,
//! MaintainerDeprecated, and the experimental InvalidDefinitionDescription). Held to
//! BuildKit by tests/oracle.rs; one deliberate difference:
//!
//! - **Suggestions are deterministic.** BuildKit suggests the first option at the least
//!   edit distance in a Go map's random order, so between equally near options its
//!   "did you mean" can change from build to build; here ties go to the option first in
//!   byte order.

use crate::go;
use crate::lint::{self, Linter, LinterView};
use crate::parser::{Heredoc, Node, Parsed};

/// The lines a node spans, one range each: BuildKit's `Location`.
pub type Location = Vec<(usize, usize)>;

/// An instruction or parse error, with BuildKit's message and the lines it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: Vec<u8>,
    pub location: Vec<Location>,
}

/// `KEY=VALUE`, or the legacy `KEY VALUE` (`no_delim`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub no_delim: bool,
}

/// An `ARG`'s name, its default if it has one, and the comment describing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgDef {
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
    pub doc_comment: Vec<u8>,
}

/// A file written from a heredoc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContent {
    pub path: Vec<u8>,
    pub data: Vec<u8>,
    pub expand: bool,
}

/// `ADD` and `COPY`'s sources and destination.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sources {
    pub dest: Vec<u8>,
    pub paths: Vec<Vec<u8>>,
    pub contents: Vec<SourceContent>,
}

/// A heredoc a shell command runs or reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineFile {
    pub name: Vec<u8>,
    pub data: Vec<u8>,
    pub chomp: bool,
}

/// `RUN`, `CMD` and `ENTRYPOINT`'s command line: the shell form joined into one word.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdLine {
    pub cmd_line: Vec<Vec<u8>>,
    /// `ENTRYPOINT` with no arguments: none, rather than empty.
    pub cmd_line_nil: bool,
    pub files: Vec<InlineFile>,
    pub prepend_shell: bool,
}

/// A `RUN --mount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub kind: Vec<u8>,
    pub from: Vec<u8>,
    pub source: Vec<u8>,
    pub target: Vec<u8>,
    pub read_only: bool,
    pub size: i64,
    pub id: Vec<u8>,
    pub sharing: Vec<u8>,
    pub required: bool,
    pub env: Option<Vec<u8>>,
    pub mode: Option<u64>,
    pub uid: Option<u64>,
    pub gid: Option<u64>,
}

/// A `RUN --device`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: Vec<u8>,
    pub required: bool,
}

/// `HEALTHCHECK`'s configuration, durations in nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Health {
    pub test: Vec<Vec<u8>>,
    pub interval: i64,
    pub timeout: i64,
    pub start_period: i64,
    pub start_interval: i64,
    pub retries: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub cmd: CmdLine,
    pub flags_used: Vec<Vec<u8>>,
    /// The mounts as parsing reads them, before expansion: only `from` is known.
    pub mounts: Vec<Mount>,
    /// Each `--mount` as written, for `parse_mount` to read whole once expanded.
    pub mount_specs: Vec<Vec<u8>>,
    pub network: Vec<u8>,
    pub security: Vec<u8>,
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Add {
    pub sources: Sources,
    pub chown: Vec<u8>,
    pub chmod: Vec<u8>,
    pub link: bool,
    pub exclude: Vec<Vec<u8>>,
    pub keep_git_dir: Option<bool>,
    pub checksum: Vec<u8>,
    pub unpack: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub sources: Sources,
    pub from: Vec<u8>,
    pub chown: Vec<u8>,
    pub chmod: Vec<u8>,
    pub link: bool,
    pub exclude: Vec<Vec<u8>>,
    pub parents: bool,
}

/// What an instruction does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Env(Vec<KeyValue>),
    Maintainer(Vec<u8>),
    Label(Vec<KeyValue>),
    Add(Add),
    Copy(Copy),
    Onbuild(Vec<u8>),
    Workdir(Vec<u8>),
    Run(Run),
    Cmd(CmdLine),
    Entrypoint(CmdLine),
    Healthcheck(Health),
    Expose(Vec<Vec<u8>>),
    User(Vec<u8>),
    Volume(Vec<Vec<u8>>),
    StopSignal(Vec<u8>),
    Arg(Vec<ArgDef>),
    Shell(Vec<Vec<u8>>),
}

/// An instruction within a stage, or an `ARG` before the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The instruction as written.
    pub name: Vec<u8>,
    /// Its line, trimmed.
    pub code: Vec<u8>,
    pub location: Location,
    pub comments: Vec<Vec<u8>>,
    pub kind: Kind,
}

/// A build stage: a `FROM` and what follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    /// Its name, lowercase, or empty.
    pub name: Vec<u8>,
    /// `FROM` as written.
    pub orig_cmd: Vec<u8>,
    pub base_name: Vec<u8>,
    pub platform: Vec<u8>,
    pub doc_comment: Vec<u8>,
    pub source_code: Vec<u8>,
    pub location: Location,
    pub comments: Vec<Vec<u8>>,
    pub commands: Vec<Command>,
}

/// A file's stages, and the `ARG`s before its first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    pub stages: Vec<Stage>,
    pub meta_args: Vec<Command>,
}

/// What one line parses to.
enum Parsed1 {
    Stage(Stage),
    Command(Command),
}

fn errf(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// The best option within edit distance 2 of `val`, ties to the option first in byte
/// order: `suggest.Search`, made deterministic (the module's documentation).
pub(crate) fn suggest(val: &[u8], options: &[&[u8]], case_sensitive: bool) -> Option<Vec<u8>> {
    let lower = |s: &[u8]| {
        if case_sensitive {
            s.to_vec()
        } else {
            go::to_lower(s)
        }
    };
    let v = lower(val);
    let mut sorted: Vec<Vec<u8>> = options.iter().map(|o| lower(o)).collect();
    sorted.sort();
    if sorted.contains(&v) {
        return None;
    }
    let mut best: Option<(usize, Vec<u8>)> = None;
    for o in sorted {
        let d = go::levenshtein(&v, &o);
        if d < best.as_ref().map_or(3, |b| b.0) {
            best = Some((d, o));
        }
    }
    let (_, m) = best?;
    Some(if case_sensitive {
        m
    } else if val == go::to_lower(val).as_slice() {
        go::to_lower(&m)
    } else if val == go::to_upper(val).as_slice() {
        go::to_upper(&m)
    } else {
        m
    })
}

/// `err` with ` (did you mean X?)` when an option is near `val`.
pub(crate) fn with_suggestion(
    mut err: Vec<u8>,
    val: &[u8],
    options: &[&[u8]],
    case_sensitive: bool,
) -> Vec<u8> {
    if let Some(m) = suggest(val, options, case_sensitive) {
        err.extend_from_slice(b" (did you mean ");
        err.extend_from_slice(&m);
        err.extend_from_slice(b"?)");
    }
    err
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlagType {
    Bool,
    String,
    Strings,
}

struct Flag {
    name: &'static str,
    kind: FlagType,
    value: Vec<u8>,
    values: Vec<Vec<u8>>,
    used: bool,
}

/// An instruction's flags: `BFlags`.
struct Flags {
    args: Vec<Vec<u8>>,
    defined: Vec<Flag>,
}

impl Flags {
    fn new(args: &[Vec<u8>]) -> Flags {
        Flags {
            args: args.to_vec(),
            defined: Vec::new(),
        }
    }

    fn add(&mut self, name: &'static str, kind: FlagType, default: &[u8]) {
        let value = match kind {
            FlagType::Bool if default == b"true" => b"true".to_vec(),
            FlagType::Bool => b"false".to_vec(),
            _ => default.to_vec(),
        };
        self.defined.push(Flag {
            name,
            kind,
            value,
            values: Vec::new(),
            used: false,
        });
    }

    fn get(&self, name: &str) -> Option<&Flag> {
        self.defined.iter().find(|f| f.name == name)
    }

    fn value(&self, name: &str) -> Vec<u8> {
        self.get(name).map(|f| f.value.clone()).unwrap_or_default()
    }

    fn is_true(&self, name: &str) -> bool {
        self.get(name).is_some_and(|f| f.value == b"true")
    }

    fn used(&self, name: &str) -> bool {
        self.get(name).is_some_and(|f| f.used)
    }

    fn values(&self, name: &str) -> Vec<Vec<u8>> {
        self.get(name).map(|f| f.values.clone()).unwrap_or_default()
    }

    /// `BFlags.Parse`.
    fn parse(&mut self) -> Result<(), Vec<u8>> {
        let args = std::mem::take(&mut self.args);
        for a in &args {
            if a == b"--" {
                return Ok(());
            }
            let Some(rest) = a.strip_prefix(b"--") else {
                return Err(errf(&[b"arg should start with -- : ", a]));
            };
            let (flag_name, value, has_value) = match a.iter().position(|&b| b == b'=') {
                Some(at) => (go::head(a, at), go::tail(a, at + 1), true),
                None => (a.as_slice(), &[][..], false),
            };
            let arg = go::tail(flag_name, 2);
            let _ = rest;
            let names: Vec<&[u8]> = self.defined.iter().map(|f| f.name.as_bytes()).collect();
            let Some(flag) = self.defined.iter_mut().find(|f| f.name.as_bytes() == arg) else {
                return Err(with_suggestion(
                    errf(&[b"unknown flag: ", flag_name]),
                    arg,
                    &names,
                    true,
                ));
            };
            if flag.used && flag.kind != FlagType::Strings {
                return Err(errf(&[b"duplicate flag specified: ", flag_name]));
            }
            flag.used = true;
            match flag.kind {
                FlagType::Bool => {
                    if has_value && value.is_empty() {
                        return Err(errf(&[b"missing a value on flag: ", flag_name]));
                    }
                    match go::to_lower(value).as_slice() {
                        b"true" | b"" => flag.value = b"true".to_vec(),
                        b"false" => flag.value = b"false".to_vec(),
                        _ => {
                            return Err(errf(&[
                                b"expecting boolean value for flag ",
                                flag_name,
                                b", not: ",
                                value,
                            ]));
                        }
                    }
                }
                FlagType::String => {
                    if !has_value {
                        return Err(errf(&[b"missing a value on flag: ", flag_name]));
                    }
                    flag.value = value.to_vec();
                }
                FlagType::Strings => {
                    if !has_value {
                        return Err(errf(&[b"missing a value on flag: ", flag_name]));
                    }
                    flag.values.push(value.to_vec());
                }
            }
        }
        Ok(())
    }
}

/// A node's arguments, an `ONBUILD`'s instruction and its arguments flattened in.
fn node_args(node: &Node) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for arg in &node.next {
        match arg.children.as_slice() {
            [] => out.push(arg.value.clone()),
            [child] => {
                out.push(child.value.clone());
                out.extend(node_args(child));
            }
            _ => {}
        }
    }
    out
}

struct Req<'a> {
    node: &'a Node,
    command: Vec<u8>,
    args: Vec<Vec<u8>>,
    flags: Flags,
    location: Location,
}

impl Req<'_> {
    fn command(&self, kind: Kind) -> Command {
        Command {
            name: self.command.clone(),
            code: go::trim_space(&self.node.original).to_vec(),
            location: self.location.clone(),
            comments: self.node.prev_comment.clone(),
            kind,
        }
    }

    fn json(&self) -> bool {
        self.node.json
    }
}

fn at_least_one(cmd: &str) -> Vec<u8> {
    format!("{cmd} requires at least one argument").into_bytes()
}

fn exactly_one(cmd: &str) -> Vec<u8> {
    format!("{cmd} requires exactly one argument").into_bytes()
}

fn no_destination(cmd: &str) -> Vec<u8> {
    format!("{cmd} requires at least two arguments, but only one was provided. Destination could not be determined").into_bytes()
}

fn blank_names(cmd: &str) -> Vec<u8> {
    format!("{cmd} names can not be blank").into_bytes()
}

fn kvps(args: &[Vec<u8>], cmd: &str) -> Result<Vec<KeyValue>, Vec<u8>> {
    if args.is_empty() {
        return Err(at_least_one(cmd));
    }
    if !args.len().is_multiple_of(3) {
        return Err(format!("Bad input to {cmd}, too many arguments").into_bytes());
    }
    args.as_chunks::<3>()
        .0
        .iter()
        .map(|c| match c {
            [k, v, d] if !k.is_empty() => Ok(KeyValue {
                key: k.clone(),
                value: v.clone(),
                no_delim: d.is_empty(),
            }),
            _ => Err(blank_names(cmd)),
        })
        .collect()
}

/// `parseSourcesAndDest`.
fn sources(req: &Req<'_>, cmd: &str) -> Result<Sources, Vec<u8>> {
    let Some((dest, srcs)) = req.args.split_last() else {
        return Err(no_destination(cmd));
    };
    let as_heredoc = |w: &[u8]| crate::parser::heredoc_word(w);
    if as_heredoc(dest).is_some() {
        return Err(format!("{cmd} cannot accept a heredoc as a destination").into_bytes());
    }
    let mut out = Sources {
        dest: dest.clone(),
        ..Sources::default()
    };
    for src in srcs {
        match as_heredoc(src) {
            Some(h) => {
                // The last heredoc of that name, as a Go map of them keeps.
                let found = req.node.heredocs.iter().rev().find(|d| d.name == h.name);
                let mut content = found.map(|d| d.content.clone()).unwrap_or_default();
                if h.chomp {
                    content = chomp(&content);
                }
                out.contents.push(SourceContent {
                    data: content,
                    path: h.name,
                    expand: h.expand,
                });
            }
            None => out.paths.push(src.clone()),
        }
    }
    Ok(out)
}

/// Leading tabs removed from every line: `ChompHeredocContent`.
fn chomp(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len());
    let mut line_start = true;
    for &b in content {
        if line_start && b == b'\t' {
            continue;
        }
        line_start = b == b'\n';
        out.push(b);
    }
    out
}

fn shell_cmd(req: &Req<'_>, empty_as_nil: bool) -> CmdLine {
    let files = req
        .node
        .heredocs
        .iter()
        .map(|h: &Heredoc| InlineFile {
            name: h.name.clone(),
            data: h.content.clone(),
            chomp: h.chomp,
        })
        .collect();
    let args = json_args(&req.args, req.json());
    CmdLine {
        cmd_line_nil: empty_as_nil && args.is_empty(),
        cmd_line: args,
        files,
        prepend_shell: !req.json(),
    }
}

/// `handleJSONArgs`: JSON as it is, the shell form as one word.
fn json_args(args: &[Vec<u8>], json: bool) -> Vec<Vec<u8>> {
    if args.is_empty() {
        return Vec::new();
    }
    if json {
        return args.to_vec();
    }
    vec![args.join(&b' ')]
}

const STAGE_NAME_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-_.";

/// `^[a-z][a-z0-9-_.]*$`.
fn valid_stage_name(n: &[u8]) -> bool {
    n.first().is_some_and(u8::is_ascii_lowercase) && n.iter().all(|b| STAGE_NAME_CHARS.contains(b))
}

/// The comment line naming `name` first, without the name: `getDocComment`.
fn doc_comment(comments: &[Vec<u8>], name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return Vec::new();
    }
    let prefix = [name, b" "].concat();
    comments
        .iter()
        .find_map(|c| c.strip_prefix(prefix.as_slice()).map(<[u8]>::to_vec))
        .unwrap_or_default()
}

/// `strings.EqualFold(word, "as")`: under Unicode simple folding, `a` is `a` or `A`, and
/// `s` is `s`, `S` or `ſ` (U+017F).
fn is_as(word: &[u8]) -> bool {
    let mut runes = go::runes(word).map(|(r, _)| r);
    matches!(
        (runes.next(), runes.next(), runes.next()),
        (Some(0x61 | 0x41), Some(0x73 | 0x53 | 0x17F), None)
    )
}

fn parse_from(req: &mut Req<'_>) -> Result<Stage, Vec<u8>> {
    let name = match req.args.as_slice() {
        [_, as_kw, name] if is_as(as_kw) => {
            let lower = go::to_lower(name);
            if !valid_stage_name(&lower) {
                return Err(errf(&[
                    b"invalid name for build stage: ",
                    go::quote(name).as_bytes(),
                    b", name can't start with a number or contain symbols",
                ]));
            }
            lower
        }
        [_] => Vec::new(),
        _ => return Err(b"FROM requires either one or three arguments".to_vec()),
    };
    req.flags.add("platform", FlagType::String, b"");
    req.flags.parse()?;
    Ok(Stage {
        base_name: req.args.first().cloned().unwrap_or_default(),
        orig_cmd: req.command.clone(),
        doc_comment: doc_comment(&req.node.prev_comment, &name),
        name,
        source_code: go::trim_space(&req.node.original).to_vec(),
        platform: req.flags.value("platform"),
        location: req.location.clone(),
        comments: req.node.prev_comment.clone(),
        commands: Vec::new(),
    })
}

/// `(?i)^\s*ONBUILD\s*` removed, `\s` being ASCII.
fn strip_onbuild(original: &[u8]) -> Vec<u8> {
    let ws = |b: u8| matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ');
    let lead = original.iter().take_while(|&&b| ws(b)).count();
    let rest = go::tail(original, lead);
    if rest.len() >= 7 && go::head(rest, 7).eq_ignore_ascii_case(b"ONBUILD") {
        let after = go::tail(rest, 7);
        let more = after.iter().take_while(|&&b| ws(b)).count();
        return go::tail(after, more).to_vec();
    }
    original.to_vec()
}

fn duration(req: &Req<'_>, name: &str) -> Result<i64, Vec<u8>> {
    let s = req.flags.value(name);
    if s.is_empty() {
        return Ok(0);
    }
    let d = go::parse_duration(&s)?;
    if d == 0 {
        return Ok(0);
    }
    if d < 1_000_000 {
        return Err(format!("Interval \"{name}\" cannot be less than 1ms").into_bytes());
    }
    Ok(d)
}

fn parse_healthcheck(req: &mut Req<'_>) -> Result<Health, Vec<u8>> {
    let Some((first, rest)) = req.args.split_first() else {
        return Err(at_least_one("HEALTHCHECK"));
    };
    let mut typ = go::to_upper(first);
    let rest = rest.to_vec();
    if typ == b"NONE" {
        if !rest.is_empty() {
            return Err(b"HEALTHCHECK NONE takes no arguments".to_vec());
        }
        return Ok(Health {
            test: vec![typ],
            ..Health::default()
        });
    }
    for name in ["interval", "timeout", "start-period", "start-interval", "retries"] {
        req.flags.add(name, FlagType::String, b"");
    }
    req.flags.parse()?;
    if typ != b"CMD" {
        return Err(errf(&[
            b"Unknown type ",
            go::quote(&typ).as_bytes(),
            b" in HEALTHCHECK (try CMD)",
        ]));
    }
    let cmd = json_args(&rest, req.json());
    if cmd.is_empty() {
        return Err(b"Missing command after HEALTHCHECK CMD".to_vec());
    }
    if !req.json() {
        typ = b"CMD-SHELL".to_vec();
    }
    let mut h = Health {
        test: std::iter::once(typ).chain(cmd).collect(),
        ..Health::default()
    };
    h.interval = duration(req, "interval")?;
    h.timeout = duration(req, "timeout")?;
    h.start_period = duration(req, "start-period")?;
    h.start_interval = duration(req, "start-interval")?;
    let retries = req.flags.value("retries");
    if !retries.is_empty() {
        let r = go::parse_int32(&retries)?;
        if r < 0 {
            return Err(format!("--retries cannot be negative ({r})").into_bytes());
        }
        h.retries = r;
    }
    Ok(h)
}

/// Expands one word of a step, as BuildKit's `SingleWordExpander`.
pub type Expand<'a> = &'a mut dyn FnMut(&[u8]) -> Result<Vec<u8>, Vec<u8>>;

/// `parseMount`: a `--mount` value, each value expanded by `expand` as the step runs.
/// Without `expand`, as parsing reads it: only `from`, which may not use variables.
pub fn parse_mount(val: &[u8], mut expand: Option<Expand<'_>>) -> Result<Mount, Vec<u8>> {
    let fields = csv_fields(val).map_err(|e| errf(&[b"failed to parse csv mounts: ", &e]))?;
    let mut m = Mount {
        kind: b"bind".to_vec(),
        from: Vec::new(),
        source: Vec::new(),
        target: Vec::new(),
        read_only: false,
        size: 0,
        id: Vec::new(),
        sharing: Vec::new(),
        required: false,
        env: None,
        mode: None,
        uid: None,
        gid: None,
    };
    let mut ro_auto = true;
    let secretish = |m: &Mount| m.kind == b"secret" || m.kind == b"ssh";
    let unexpected =
        |key: &[u8], m: &Mount| errf(&[b"unexpected key '", key, b"' for mount type '", &m.kind, b"'"]);
    let invalid = |key: &[u8], value: &[u8]| errf(&[b"invalid value for ", key, b": ", value]);
    for field in &fields {
        let (key, value) = match field.iter().position(|&b| b == b'=') {
            Some(at) => (go::to_lower(go::head(field, at)), Some(go::tail(field, at + 1))),
            None => (go::to_lower(field), None),
        };
        let Some(value) = value else {
            if expand.is_none() {
                continue;
            }
            match key.as_slice() {
                b"readonly" | b"ro" => {
                    m.read_only = true;
                    ro_auto = false;
                }
                b"readwrite" | b"rw" => {
                    m.read_only = false;
                    ro_auto = false;
                }
                b"required" if secretish(&m) => m.required = true,
                b"required" => return Err(unexpected(&key, &m)),
                _ => return Err(errf(&[b"invalid field '", field, b"' must be a key=value pair"])),
            }
            continue;
        };
        let value = match expand.as_mut() {
            Some(e) => e(value)?,
            None if key == b"from" => {
                if let Some(i) = value.iter().position(|&b| b == b'$')
                    && i != value.len() - 1
                {
                    return Err(
                        b"'from' doesn't support variable expansion, define alias stage instead".to_vec(),
                    );
                }
                value.to_vec()
            }
            None => continue,
        };
        match key.as_slice() {
            b"type" => {
                let v = go::to_lower(&value);
                if !matches!(v.as_slice(), b"bind" | b"cache" | b"tmpfs" | b"secret" | b"ssh") {
                    let e = errf(&[b"unsupported mount type ", go::quote(&value).as_bytes()]);
                    return Err(with_suggestion(
                        e,
                        &value,
                        &[b"bind", b"cache", b"tmpfs", b"secret", b"ssh"],
                        true,
                    ));
                }
                m.kind = v;
            }
            b"from" => m.from = value,
            b"source" | b"src" => m.source = value,
            b"target" | b"dst" | b"destination" => m.target = value,
            b"readonly" | b"ro" => {
                m.read_only = go::parse_bool(&value).ok_or_else(|| invalid(&key, &value))?;
                ro_auto = false;
            }
            b"readwrite" | b"rw" => {
                m.read_only = !go::parse_bool(&value).ok_or_else(|| invalid(&key, &value))?;
                ro_auto = false;
            }
            b"required" if secretish(&m) => {
                m.required = go::parse_bool(&value).ok_or_else(|| invalid(&key, &value))?;
            }
            b"required" => return Err(unexpected(&key, &m)),
            b"size" if m.kind == b"tmpfs" => {
                m.size = go::ram_in_bytes(&value).map_err(|_| invalid(&key, &value))?;
            }
            b"size" => return Err(unexpected(&key, &m)),
            b"id" => m.id = value,
            b"sharing" => {
                let v = go::to_lower(&value);
                if !matches!(v.as_slice(), b"shared" | b"private" | b"locked") {
                    let e = errf(&[b"unsupported sharing value ", go::quote(&value).as_bytes()]);
                    return Err(with_suggestion(
                        e,
                        &value,
                        &[b"shared", b"private", b"locked"],
                        true,
                    ));
                }
                m.sharing = v;
            }
            b"mode" => {
                let bad = || errf(&[b"invalid value ", &value, b" for mode"]);
                m.mode = Some(parse_uint32(&value, 8).ok_or_else(bad)?);
            }
            b"uid" => {
                let bad = || errf(&[b"invalid value ", &value, b" for uid"]);
                m.uid = Some(parse_uint32(&value, 10).ok_or_else(bad)?);
            }
            b"gid" => {
                let bad = || errf(&[b"invalid value ", &value, b" for gid"]);
                m.gid = Some(parse_uint32(&value, 10).ok_or_else(bad)?);
            }
            b"env" => m.env = Some(value),
            _ => {
                let all: &[&[u8]] = &[
                    b"type",
                    b"from",
                    b"source",
                    b"target",
                    b"readonly",
                    b"id",
                    b"sharing",
                    b"required",
                    b"size",
                    b"mode",
                    b"uid",
                    b"gid",
                    b"src",
                    b"dst",
                    b"destination",
                    b"ro",
                    b"rw",
                    b"readwrite",
                    b"env",
                ];
                let e = errf(&[b"unexpected key '", &key, b"' in '", field, b"'"]);
                return Err(with_suggestion(e, &key, all, true));
            }
        }
    }
    let file_info = matches!(m.kind.as_slice(), b"secret" | b"ssh" | b"cache");
    if !file_info {
        for (set, what) in [
            (m.mode.is_some(), "mode"),
            (m.uid.is_some(), "uid"),
            (m.gid.is_some(), "gid"),
        ] {
            if set {
                return Err(errf(&[
                    what.as_bytes(),
                    b" not allowed for ",
                    go::quote(&m.kind).as_bytes(),
                    b" type mounts",
                ]));
            }
        }
    }
    if ro_auto {
        m.read_only = !matches!(m.kind.as_slice(), b"cache" | b"tmpfs");
    }
    if m.kind == b"secret" {
        if !m.from.is_empty() {
            return Err(b"secret mount should not have a from".to_vec());
        }
        if !m.sharing.is_empty() {
            return Err(b"secret mount should not define sharing".to_vec());
        }
        if m.source.is_empty() && m.target.is_empty() && m.id.is_empty() {
            return Err(b"invalid secret mount. one of source, target required".to_vec());
        }
        if !m.source.is_empty() && !m.id.is_empty() {
            return Err(b"both source and id can't be set".to_vec());
        }
    }
    if !m.sharing.is_empty() && m.kind != b"cache" {
        return Err(errf(&[b"invalid cache sharing set for ", &m.kind, b" mount"]));
    }
    Ok(m)
}

/// `strconv.ParseUint(s, base, 32)`, for base 8 or 10: digits of the base only, no sign
/// and no underscores (a base given, Go allows neither).
fn parse_uint32(s: &[u8], base: u32) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &c in s {
        let d = u64::from(char::from(c).to_digit(base)?);
        n = n.checked_mul(u64::from(base))?.checked_add(d)?;
        if n > u64::from(u32::MAX) {
            return None;
        }
    }
    Some(n)
}

/// `ParseDevice`.
fn parse_device(val: &[u8]) -> Result<Device, Vec<u8>> {
    let fields = csv_fields(val).map_err(|e| errf(&[b"failed to parse csv devices: ", &e]))?;
    let mut d = Device {
        name: Vec::new(),
        required: false,
    };
    for field in &fields {
        let (key, value) = match field.iter().position(|&b| b == b'=') {
            Some(at) => (go::to_lower(go::head(field, at)), Some(go::tail(field, at + 1))),
            None => (go::to_lower(field), None),
        };
        let Some(value) = value else {
            if key == b"required" {
                d.required = true;
            } else if d.name.is_empty() {
                d.name = field.clone();
            } else {
                return Err(errf(&[b"invalid field '", field, b"' must be a key=value pair"]));
            }
            continue;
        };
        match key.as_slice() {
            b"name" => {
                if !d.name.is_empty() {
                    return Err(errf(&[b"device name already set to ", &d.name]));
                }
                d.name = value.to_vec();
            }
            b"required" => {
                d.required = go::parse_bool(value)
                    .ok_or_else(|| errf(&[b"invalid value for ", &key, b": ", value]))?;
            }
            _ => {
                if d.name.is_empty() {
                    d.name = field.clone();
                    continue;
                }
                let e = errf(&[b"unexpected key '", &key, b"' in '", field, b"'"]);
                return Err(with_suggestion(e, &key, &[b"name", b"required"], true));
            }
        }
    }
    Ok(d)
}

/// One CSV record's fields, as tonistiigi/go-csvvalue reads them (Go's `encoding/csv`
/// rules for one line), with `csv.ParseError`'s text on error.
pub(crate) fn csv_fields(line: &[u8]) -> Result<Vec<Vec<u8>>, Vec<u8>> {
    let err =
        |pos: usize, what: &str| format!("parse error on line 1, column {}: {what}", pos + 1).into_bytes();
    let mut line = line;
    if line.last() == Some(&b'\n') {
        line = if line.len() > 1 && line.get(line.len() - 2) == Some(&b'\r') {
            go::head(line, line.len() - 2)
        } else {
            go::head(line, line.len() - 1)
        };
    }
    if line.is_empty() {
        return Err(b"EOF".to_vec());
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut pos = 0usize;
    loop {
        if line.first() != Some(&b'"') {
            let i = line.iter().position(|&b| b == b',');
            let field = match i {
                Some(i) => go::head(line, i),
                None => line,
            };
            if let Some(j) = field.iter().position(|&b| b == b'"') {
                return Err(err(pos + j, "bare \" in non-quoted-field"));
            }
            out.push(field.to_vec());
            match i {
                Some(i) => {
                    line = go::tail(line, i + 1);
                    pos += i + 1;
                    continue;
                }
                None => break,
            }
        }
        line = go::tail(line, 1);
        pos += 1;
        // After a doubled quote, the next piece continues the same field.
        let mut half_open = false;
        loop {
            let Some(i) = line.iter().position(|&b| b == b'"') else {
                return Err(err(pos, "extraneous or missing \" in quoted-field"));
            };
            let piece = go::head(line, i).to_vec();
            if half_open {
                if let Some(last) = out.last_mut() {
                    last.extend_from_slice(&piece);
                }
            } else {
                out.push(piece);
            }
            line = go::tail(line, i + 1);
            pos += i + 1;
            match line.first() {
                Some(b'"') => {
                    if let Some(last) = out.last_mut() {
                        last.push(b'"');
                    }
                    line = go::tail(line, 1);
                    pos += 1;
                    half_open = true;
                }
                Some(b',') => {
                    line = go::tail(line, 1);
                    pos += 1;
                    break;
                }
                None => return Ok(out),
                Some(_) => return Err(err(pos - 1, "extraneous or missing \" in quoted-field")),
            }
        }
    }
    Ok(out)
}

fn parse_run(req: &mut Req<'_>) -> Result<Run, Vec<u8>> {
    // In BuildKit's hook order: devices, mounts, network, security.
    req.flags.add("device", FlagType::Strings, b"");
    req.flags.add("mount", FlagType::Strings, b"");
    req.flags.add("network", FlagType::String, b"default");
    req.flags.add("security", FlagType::String, b"sandbox");
    req.flags.parse()?;
    let mut flags_used: Vec<Vec<u8>> = req
        .flags
        .defined
        .iter()
        .filter(|f| f.used)
        .map(|f| f.name.as_bytes().to_vec())
        .collect();
    flags_used.sort();
    let cmd = shell_cmd(req, false);
    let devices = req
        .flags
        .values("device")
        .iter()
        .map(|d| parse_device(d))
        .collect::<Result<Vec<_>, _>>()?;
    let mounts = req
        .flags
        .values("mount")
        .iter()
        .map(|m| parse_mount(m, None))
        .collect::<Result<Vec<_>, _>>()?;
    let network = req.flags.value("network");
    if !matches!(network.as_slice(), b"default" | b"none" | b"host") {
        return Err(errf(&[b"invalid network mode ", go::quote(&network).as_bytes()]));
    }
    let security = req.flags.value("security");
    if !matches!(security.as_slice(), b"insecure" | b"sandbox") {
        return Err(errf(&[
            b"security ",
            go::quote(&security).as_bytes(),
            b" is not valid",
        ]));
    }
    let mount_specs = req.flags.values("mount");
    Ok(Run {
        cmd,
        flags_used,
        mounts,
        mount_specs,
        network,
        security,
        devices,
    })
}

const INSTRUCTIONS: [&[u8]; 18] = [
    b"ADD",
    b"ARG",
    b"CMD",
    b"COPY",
    b"ENTRYPOINT",
    b"ENV",
    b"EXPOSE",
    b"FROM",
    b"HEALTHCHECK",
    b"LABEL",
    b"MAINTAINER",
    b"ONBUILD",
    b"RUN",
    b"SHELL",
    b"STOPSIGNAL",
    b"USER",
    b"VOLUME",
    b"WORKDIR",
];

/// One node: `ParseInstructionWithLinter`.
fn instruction(node: &Node, lint: &Linter) -> Result<Parsed1, Vec<u8>> {
    let lint = lint.with_comments(&node.prev_comment);
    let location: Location = (node.start_line..=node.end_line.max(node.start_line))
        .map(|l| (l, l))
        .collect();
    let mut req = Req {
        node,
        command: node.value.clone(),
        args: node_args(node),
        flags: Flags::new(&node.flags),
        location: location.clone(),
    };
    let lower = go::to_lower(&node.value);
    let kind = match lower.as_slice() {
        b"env" => {
            req.flags.parse()?;
            Kind::Env(kvps(&req.args, "ENV")?)
        }
        b"maintainer" => {
            lint.run(
                &lint::MAINTAINER_DEPRECATED,
                &location,
                Some(b"Maintainer instruction is deprecated in favor of using label"),
            );
            let [m] = req.args.as_slice() else {
                return Err(exactly_one("MAINTAINER"));
            };
            let m = m.clone();
            req.flags.parse()?;
            Kind::Maintainer(m)
        }
        b"label" => {
            req.flags.parse()?;
            Kind::Label(kvps(&req.args, "LABEL")?)
        }
        b"add" => {
            if req.args.len() < 2 {
                return Err(no_destination("ADD"));
            }
            req.flags.add("chown", FlagType::String, b"");
            req.flags.add("chmod", FlagType::String, b"");
            req.flags.add("link", FlagType::Bool, b"false");
            req.flags.add("keep-git-dir", FlagType::Bool, b"false");
            req.flags.add("checksum", FlagType::String, b"");
            req.flags.add("unpack", FlagType::Bool, b"false");
            req.flags.add("exclude", FlagType::Strings, b"");
            req.flags.parse()?;
            let f = &req.flags;
            Kind::Add(Add {
                sources: sources(&req, "ADD")?,
                chown: f.value("chown"),
                chmod: f.value("chmod"),
                link: f.is_true("link"),
                exclude: f.values("exclude"),
                keep_git_dir: f.used("keep-git-dir").then(|| f.is_true("keep-git-dir")),
                checksum: f.value("checksum"),
                unpack: f.used("unpack").then(|| f.is_true("unpack")),
            })
        }
        b"copy" => {
            if req.args.len() < 2 {
                return Err(no_destination("COPY"));
            }
            req.flags.add("chown", FlagType::String, b"");
            req.flags.add("from", FlagType::String, b"");
            req.flags.add("chmod", FlagType::String, b"");
            req.flags.add("link", FlagType::Bool, b"false");
            req.flags.add("exclude", FlagType::Strings, b"");
            req.flags.add("parents", FlagType::Bool, b"false");
            req.flags.parse()?;
            let f = &req.flags;
            Kind::Copy(Copy {
                sources: sources(&req, "COPY")?,
                from: f.value("from"),
                chown: f.value("chown"),
                chmod: f.value("chmod"),
                link: f.is_true("link"),
                exclude: f.values("exclude"),
                parents: f.is_true("parents"),
            })
        }
        b"from" => {
            if let [_, _, name] = req.args.as_slice()
                && *name != go::to_lower(name)
            {
                let msg = errf(&[b"Stage name '", name, b"' should be lowercase"]);
                lint.run(&lint::STAGE_NAME_CASING, &location, Some(&msg));
            }
            if !from_case_matches_as(&req) {
                let as_kw = req.args.get(1).cloned().unwrap_or_default();
                let msg = errf(&[
                    b"'",
                    &as_kw,
                    b"' and '",
                    &req.command,
                    b"' keywords' casing do not match",
                ]);
                lint.run(&lint::FROM_AS_CASING, &location, Some(&msg));
            }
            let stage = parse_from(&mut req)?;
            if !stage.name.is_empty() {
                definition_description(
                    &lint,
                    "FROM",
                    std::slice::from_ref(&stage.name),
                    &node.prev_comment,
                    &location,
                );
            }
            return Ok(Parsed1::Stage(stage));
        }
        b"onbuild" => {
            let Some(first) = req.args.first() else {
                return Err(at_least_one("ONBUILD"));
            };
            let trigger = go::to_upper(go::trim_space(first));
            req.flags.parse()?;
            match trigger.as_slice() {
                b"ONBUILD" => return Err(b"Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed".to_vec()),
                b"MAINTAINER" | b"FROM" => {
                    return Err(errf(&[&trigger, b" isn't allowed as an ONBUILD trigger"]));
                }
                _ => {}
            }
            let mut expr = strip_onbuild(&node.original);
            for h in &node.heredocs {
                expr.push(b'\n');
                expr.extend_from_slice(&h.content);
                expr.extend_from_slice(&h.name);
            }
            Kind::Onbuild(expr)
        }
        b"workdir" => {
            let [p] = req.args.as_slice() else {
                return Err(exactly_one("WORKDIR"));
            };
            let p = p.clone();
            req.flags.parse()?;
            Kind::Workdir(p)
        }
        b"run" => Kind::Run(parse_run(&mut req)?),
        b"cmd" => {
            req.flags.parse()?;
            Kind::Cmd(shell_cmd(&req, false))
        }
        b"healthcheck" => Kind::Healthcheck(parse_healthcheck(&mut req)?),
        b"entrypoint" => {
            req.flags.parse()?;
            Kind::Entrypoint(shell_cmd(&req, true))
        }
        b"expose" => {
            if req.args.is_empty() {
                return Err(at_least_one("EXPOSE"));
            }
            req.flags.parse()?;
            let mut ports = req.args.clone();
            ports.sort();
            Kind::Expose(ports)
        }
        b"user" => {
            let [u] = req.args.as_slice() else {
                return Err(exactly_one("USER"));
            };
            let u = u.clone();
            req.flags.parse()?;
            Kind::User(u)
        }
        b"volume" => {
            if req.args.is_empty() {
                return Err(at_least_one("VOLUME"));
            }
            req.flags.parse()?;
            let mut vols = Vec::with_capacity(req.args.len());
            for v in &req.args {
                let v = go::trim_space(v);
                if v.is_empty() {
                    return Err(b"VOLUME specified can not be an empty string".to_vec());
                }
                vols.push(v.to_vec());
            }
            Kind::Volume(vols)
        }
        b"stopsignal" => {
            // BuildKit parses no flags for STOPSIGNAL.
            let [s] = req.args.as_slice() else {
                return Err(exactly_one("STOPSIGNAL"));
            };
            Kind::StopSignal(s.clone())
        }
        b"arg" => {
            if req.args.is_empty() {
                return Err(at_least_one("ARG"));
            }
            let mut defs = Vec::with_capacity(req.args.len());
            for a in &req.args {
                let (key, value) = match a.iter().position(|&b| b == b'=') {
                    Some(at) => {
                        if at == 0 {
                            return Err(blank_names("ARG"));
                        }
                        (go::head(a, at).to_vec(), Some(go::tail(a, at + 1).to_vec()))
                    }
                    None => (a.clone(), None),
                };
                defs.push(ArgDef {
                    doc_comment: doc_comment(&node.prev_comment, &key),
                    key,
                    value,
                });
            }
            let keys: Vec<Vec<u8>> = defs.iter().map(|d| d.key.clone()).collect();
            definition_description(&lint, "ARG", &keys, &node.prev_comment, &location);
            Kind::Arg(defs)
        }
        b"shell" => {
            req.flags.parse()?;
            let words = json_args(&req.args, req.json());
            if words.is_empty() {
                return Err(at_least_one("SHELL"));
            }
            if !req.json() {
                return Err(errf(&[b"SHELL requires the arguments to be in JSON form"]));
            }
            Kind::Shell(words)
        }
        _ => {
            let e = errf(&[b"unknown instruction: ", &node.value]);
            return Err(with_suggestion(e, &node.value, &INSTRUCTIONS, false));
        }
    };
    Ok(Parsed1::Command(req.command(kind)))
}

/// `doesFromCaseMatchAsCase`.
fn from_case_matches_as(req: &Req<'_>) -> bool {
    let Some(as_kw) = req.args.get(1).filter(|_| req.args.len() >= 3) else {
        return true;
    };
    let lower = req.command == go::to_lower(&req.command);
    let upper = req.command == go::to_upper(&req.command);
    if !lower && !upper {
        return true;
    }
    if lower {
        *as_kw == go::to_lower(as_kw)
    } else {
        *as_kw == go::to_upper(as_kw)
    }
}

/// `validateDefinitionDescription`.
fn definition_description(
    lint: &LinterView<'_>,
    instruction: &str,
    keys: &[Vec<u8>],
    comments: &[Vec<u8>],
    location: &Location,
) {
    let (Some(last), Some(first_key)) = (comments.last(), keys.first()) else {
        return;
    };
    let word = last.split(|&b| b == b' ').next().unwrap_or_default();
    if keys.iter().any(|k| k.as_slice() == word) {
        return;
    }
    let example: &[u8] = if keys.len() > 1 { b"<arg_key>" } else { first_key };
    let msg = errf(&[
        b"Comment for ",
        instruction.as_bytes(),
        b" should follow the format: `# ",
        example,
        b" <description>`",
    ]);
    lint.run(&lint::INVALID_DEFINITION_DESCRIPTION, location, Some(&msg));
}

/// A file's stages and the `ARG`s before them: `instructions.Parse`. Errors are
/// BuildKit's: `dockerfile parse error on line N: ...`, located at the instruction.
/// `ParseCommand`: one node as an instruction within a stage, with no build checks; a
/// `FROM` is not one.
pub fn parse_command(node: &crate::parser::Node) -> Result<Command, Error> {
    let location: Location = (node.start_line..=node.end_line.max(node.start_line))
        .map(|l| (l, l))
        .collect();
    let fail = |message: Vec<u8>| Error {
        message,
        location: vec![location.clone()],
    };
    match instruction(node, &Linter::default()).map_err(fail)? {
        Parsed1::Command(c) => Ok(c),
        Parsed1::Stage(_) => Err(fail(b"*instructions.Stage is not a command type".to_vec())),
    }
}

pub fn parse(parsed: &Parsed, lint: &Linter) -> Result<Instructions, Error> {
    let mut stages: Vec<Stage> = Vec::new();
    let mut meta_args = Vec::new();
    for node in &parsed.instructions {
        let location: Location = (node.start_line..=node.end_line.max(node.start_line))
            .map(|l| (l, l))
            .collect();
        let one = instruction(node, lint).map_err(|m| {
            let mut message = format!("dockerfile parse error on line {}: ", node.start_line).into_bytes();
            message.extend_from_slice(&m);
            Error {
                message,
                location: vec![location.clone()],
            }
        })?;
        match one {
            Parsed1::Stage(s) => stages.push(s),
            Parsed1::Command(c) => {
                if stages.is_empty() && matches!(c.kind, Kind::Arg(_)) {
                    meta_args.push(c);
                    continue;
                }
                let Some(stage) = stages.last_mut() else {
                    return Err(Error {
                        message: b"no build stage in current context".to_vec(),
                        location: vec![location],
                    });
                };
                stage.commands.push(c);
            }
        }
    }
    Ok(Instructions { stages, meta_args })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Ties between equally near options go to the first in byte order, every time.
    #[test]
    fn suggestions_are_deterministic() {
        assert_eq!(suggest(b"ab", &[b"ac", b"aa"], true), Some(b"aa".to_vec()));
        assert_eq!(suggest(b"ab", &[b"aa", b"ac"], true), Some(b"aa".to_vec()));
        assert_eq!(
            suggest(b"run", &[b"RUN"], false),
            None,
            "an exact match suggests nothing"
        );
        assert_eq!(suggest(b"RUNN", &INSTRUCTIONS, false), Some(b"RUN".to_vec()));
        assert_eq!(suggest(b"zzzzzz", &[b"chmod"], true), None);
    }

    /// CSV fields as Go's encoding/csv reads one record.
    #[test]
    fn csv_reads_as_go_reads() {
        assert_eq!(
            csv_fields(b"a,\"b,c\",d").unwrap(),
            vec![b"a".to_vec(), b"b,c".to_vec(), b"d".to_vec()]
        );
        assert_eq!(csv_fields(b"\"a\"\"b\"").unwrap(), vec![b"a\"b".to_vec()]);
        assert_eq!(csv_fields(b"a,").unwrap(), vec![b"a".to_vec(), b"".to_vec()]);
        assert_eq!(
            csv_fields(b"a\"b").unwrap_err(),
            b"parse error on line 1, column 2: bare \" in non-quoted-field".to_vec()
        );
        assert_eq!(
            csv_fields(b"\"ab").unwrap_err(),
            b"parse error on line 1, column 2: extraneous or missing \" in quoted-field".to_vec()
        );
        assert_eq!(
            csv_fields(b"\"a\"b").unwrap_err(),
            b"parse error on line 1, column 3: extraneous or missing \" in quoted-field".to_vec()
        );
        assert_eq!(csv_fields(b"").unwrap_err(), b"EOF".to_vec());
    }
}
