//! A build's plan as BuildKit's LLB holds it (`client/llb`, `solver/pb` of moby/buildkit
//! dockerfile/1.27.1): states that carry an output and the environment, directory, user
//! and platform the next step runs with; vertices (image, context and other sources,
//! commands, file operations, merges) built from them; and the definition that marshals
//! from a state, each operation once however many vertices make it, in an order where
//! every input comes first, with its metadata merged as BuildKit merges it.
//!
//! Strings are Go strings, bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::go;
use crate::platform::Platform;

/// A vertex in the plan.
pub type VertexId = usize;

/// One output of a vertex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Output {
    pub vertex: VertexId,
    pub index: i64,
}

/// `pb.NetMode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum NetMode {
    #[default]
    Sandbox,
    Host,
    None,
}

/// `pb.SecurityMode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Security {
    #[default]
    Sandbox,
    Insecure,
}

/// `pb.LinuxResources`: a step's limits (the frontend's `memory`, `cpushares` and the
/// like); zero, or empty, where unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinuxResources {
    pub memory: i64,
    pub memory_swap: i64,
    pub cpu_shares: u64,
    pub cpu_period: u64,
    pub cpu_quota: i64,
    pub cpuset_cpus: Vec<u8>,
    pub cpuset_mems: Vec<u8>,
}

/// A vertex's metadata: what progress shows for it, and how the cache treats it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub ignore_cache: bool,
    pub description: BTreeMap<Vec<u8>, Vec<u8>>,
    /// `pb.ExportCache`'s value, which only a definition given whole sets here.
    pub export_cache: Option<bool>,
    /// The capabilities a definition given whole names in an op's metadata, beside those
    /// its kind needs ([`crate::caps::op`]); each said to be needed, as `mergeMetadata`
    /// says every one it merges.
    pub caps: BTreeSet<Vec<u8>>,
    pub progress_group: Option<ProgressGroup>,
    pub linux_resources: Option<LinuxResources>,
    /// Where in the Dockerfile it comes from (`llb.SourceMap.Location`): each a set of
    /// lines, of every vertex marshalled to the op, in the order they were (D80).
    pub locations: Vec<crate::instructions::Location>,
    /// Where in the source maps of definitions given whole it comes from.
    pub given_locations: Vec<GivenLocation>,
}

impl Meta {
    /// `mergeMetadata`: `other` over `self`.
    fn merge(&mut self, other: &Meta) {
        self.ignore_cache |= other.ignore_cache;
        for (k, v) in &other.description {
            self.description.insert(k.clone(), v.clone());
        }
        if other.export_cache.is_some() {
            self.export_cache = other.export_cache;
        }
        self.caps.extend(other.caps.iter().cloned());
        if other.progress_group.is_some() {
            self.progress_group.clone_from(&other.progress_group);
        }
        if other.linux_resources.is_some() {
            self.linux_resources.clone_from(&other.linux_resources);
        }
        // sourceMapCollector.Add: each vertex's locations, appended after those before.
        let before = self.locations.len();
        self.given_locations
            .extend(other.given_locations.iter().map(|g| GivenLocation {
                after: before + g.after,
                ..g.clone()
            }));
        self.locations.extend(other.locations.iter().cloned());
    }
}

/// `pb.ProgressGroup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressGroup {
    pub id: GroupId,
    pub name: Vec<u8>,
    pub weak: bool,
}

/// A progress group's ID: the group's place among the plan's groups, where BuildKit
/// draws a random one; or, of a definition given whole, the one it came with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupId {
    Ours(usize),
    Given(Vec<u8>),
}

/// `pb.Position`, as a definition given whole has one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    pub line: i32,
    pub character: i32,
}

/// `pb.Range`: where it starts and ends, each where set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Range {
    pub start: Option<Position>,
    pub end: Option<Position>,
}

/// A location in a source map of a definition given whole: the map (one of
/// [`Graph::sources`]), its ranges, and how many of the op's own [`Meta::locations`] come
/// before it, which keeps the order `sourceMapCollector.Add` appends them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenLocation {
    pub source: usize,
    pub ranges: Vec<Range>,
    pub after: usize,
}

/// `pb.ChownOpt`: a user and a group, each by ID or by a name looked up in the target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Chown {
    pub user: Option<UserOpt>,
    pub group: Option<UserOpt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserOpt {
    Id(u32),
    Name(Vec<u8>),
}

impl Chown {
    /// `llb.WithUser`: `user[:group]`, each a number (or `root`) or a name.
    pub fn parse(spec: &[u8]) -> Chown {
        let opt = |v: &[u8]| {
            if v == b"root" {
                return UserOpt::Id(0);
            }
            match go::parse_int32(v) {
                // A negative ID wraps as Go's uint32 conversion wraps it.
                Ok(n) => UserOpt::Id(n as u32),
                Err(_) => UserOpt::Name(v.to_vec()),
            }
        };
        match spec.iter().position(|&b| b == b':') {
            Some(at) => Chown {
                user: Some(opt(go::head(spec, at))),
                group: Some(opt(go::tail(spec, at + 1))),
            },
            None => Chown {
                user: Some(opt(spec)),
                group: None,
            },
        }
    }
}

/// `llb.ChmodOpt`: a mode in octal, or as a string such as `u+x`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Chmod {
    Mode(u32),
    Str(Vec<u8>),
}

/// `llb.CopyInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CopyInfo {
    pub mode: Option<Chmod>,
    pub follow_symlinks: bool,
    pub dir_contents_only: bool,
    pub include_patterns: Vec<Vec<u8>>,
    pub exclude_patterns: Vec<Vec<u8>>,
    pub required_paths: Vec<Vec<u8>>,
    pub attempt_unpack: bool,
    pub create_dest_path: bool,
    pub allow_wildcard: bool,
    pub allow_empty_wildcard: bool,
    pub chown: Option<Chown>,
    /// Nanoseconds since the epoch, for every file it writes.
    pub created: Option<i64>,
}

/// A file action, before it is bound to the state it runs on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Mkdir {
        path: Vec<u8>,
        mode: u32,
        make_parents: bool,
        chown: Option<Chown>,
        created: Option<i64>,
    },
    Mkfile {
        path: Vec<u8>,
        mode: u32,
        data: Vec<u8>,
        chown: Option<Chown>,
        created: Option<i64>,
    },
    Copy {
        /// The state copied from: its output (none for scratch) and directory.
        from: Option<Output>,
        from_dir: Vec<u8>,
        src: Vec<u8>,
        dest: Vec<u8>,
        info: CopyInfo,
    },
}

/// A mount of a command.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Mount {
    pub target: Vec<u8>,
    pub source: Option<Output>,
    pub readonly: bool,
    pub selector: Vec<u8>,
    pub kind: MountKind,
    /// `llb.ForceNoOutput`: writable, its changes dropped.
    pub no_output: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MountKind {
    Bind,
    Cache { id: Vec<u8>, sharing: Sharing },
    Tmpfs { size: i64 },
}

/// `pb.CacheSharingOpt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sharing {
    Shared,
    Private,
    Locked,
}

/// A secret, to a file or into a variable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Secret {
    pub id: Vec<u8>,
    pub target: Option<Vec<u8>>,
    pub env: Option<Vec<u8>>,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub optional: bool,
}

/// An SSH agent socket.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ssh {
    pub id: Vec<u8>,
    pub target: Vec<u8>,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub optional: bool,
}

/// `pb.ProxyEnv`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyEnv {
    pub http: Vec<u8>,
    pub https: Vec<u8>,
    pub ftp: Vec<u8>,
    pub no: Vec<u8>,
    pub all: Vec<u8>,
}

/// `pb.HostIP`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostIp {
    pub host: Vec<u8>,
    pub ip: Vec<u8>,
}

/// `pb.Ulimit`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ulimit {
    pub name: Vec<u8>,
    pub soft: i64,
    pub hard: i64,
}

/// `pb.CDIDevice`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Device {
    pub name: Vec<u8>,
    pub optional: bool,
}

/// A command's process: `pb.Meta`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Process {
    pub args: Vec<Vec<u8>>,
    pub env: Vec<Vec<u8>>,
    pub cwd: Vec<u8>,
    pub user: Vec<u8>,
    pub proxy: Option<ProxyEnv>,
    pub extra_hosts: Vec<HostIp>,
    pub hostname: Vec<u8>,
    pub ulimits: Vec<Ulimit>,
    pub cgroup_parent: Vec<u8>,
}

/// What a vertex does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Source {
        identifier: Vec<u8>,
        attrs: BTreeMap<Vec<u8>, Vec<u8>>,
    },
    Exec {
        process: Box<Process>,
        /// Sorted by target, the root first among equals.
        mounts: Vec<Mount>,
        network: NetMode,
        security: Security,
        secrets: Vec<Secret>,
        ssh: Vec<Ssh>,
        devices: Vec<Device>,
    },
    File {
        /// The state the actions run on: its output (none for scratch) and directory.
        base: Option<Output>,
        base_dir: Vec<u8>,
        actions: Vec<Action>,
    },
    Merge {
        inputs: Vec<Output>,
    },
    /// shards' own (D54): the skills a tree holds, each checked as the Agent Skills
    /// reference checks it and laid out in a directory of its name. `name` is the
    /// directory a source that is one skill came as, for the check of its name.
    Skills {
        input: Output,
        name: Vec<u8>,
    },
    /// An operation of a definition given whole, as `llb.NewDefinitionOp` keeps one (a
    /// frontend's input): the bytes it was marshalled as, written again as they are, so
    /// that its digest and those its readers name stay what they were; the outputs it
    /// reads, each another such operation; and, where it is one, the source it is. Its
    /// metadata and locations are the vertex's [`Meta`].
    Raw {
        bytes: Vec<u8>,
        inputs: Vec<Output>,
        source: Option<RawSource>,
    },
}

/// The source an operation given whole is (`pb.SourceOp`): its identifier and attributes.
pub type RawSource = (Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>);

/// A definition given whole (`pb.Definition`), as a frontend's input is: its operations,
/// the last its root, which names its output; and the source maps their locations name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Embedded {
    pub ops: Vec<EmbeddedOp>,
    pub sources: Vec<GivenSource>,
}

/// An operation of an [`Embedded`]: its bytes, what it reads (a digest and an output
/// each, as its bytes name them), its metadata where the definition has any for it, its
/// source locations (an index into the definition's sources, and the location's ranges)
/// and, where it is one, the source it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddedOp {
    pub bytes: Vec<u8>,
    pub inputs: Vec<(Vec<u8>, i64)>,
    pub metadata: Option<Meta>,
    pub locations: Vec<(usize, Vec<Range>)>,
    pub source: Option<RawSource>,
}

/// A source map of a definition given whole (`pb.SourceInfo`): its file, data and
/// language, and the definition that loads it as `sourceMapCollector.Marshal` writes it
/// again (the one it came with, marshalled anew), as protobuf and as encoding/json writes
/// it, where it came with one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GivenSource {
    pub filename: Vec<u8>,
    pub data: Vec<u8>,
    pub language: Vec<u8>,
    pub definition: Option<(Vec<u8>, String)>,
}

/// A source map an op is located in: the Dockerfile's, or one of [`Graph::sources`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceRef {
    Own,
    Given(usize),
}

/// A vertex: what it does, the platform it runs for (sources of images and commands
/// carry one), and its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vertex {
    pub kind: Kind,
    pub platform: Option<Platform>,
    pub meta: Meta,
}

impl Vertex {
    /// `Vertex.Inputs()`: the outputs it reads, once each, in the order BuildKit's marshal
    /// visits them.
    fn inputs(&self) -> Vec<Output> {
        let mut out: Vec<Output> = Vec::new();
        let mut add = |o: Output| {
            if !out.contains(&o) {
                out.push(o);
            }
        };
        match &self.kind {
            Kind::Source { .. } => {}
            Kind::Exec { mounts, .. } => mounts.iter().filter_map(|m| m.source).for_each(add),
            // FileAction.allOutputs: from the last action back, each its state, then
            // what it copies from.
            Kind::File { base, actions, .. } => {
                for a in actions.iter().rev() {
                    if let Some(b) = base {
                        add(*b);
                    }
                    if let Action::Copy { from: Some(f), .. } = a {
                        add(*f);
                    }
                }
            }
            Kind::Merge { inputs } => inputs.iter().copied().for_each(add),
            Kind::Skills { input, .. } => add(*input),
            Kind::Raw { inputs, .. } => inputs.iter().copied().for_each(add),
        }
        out
    }
}

/// An environment as `llb.EnvList` keeps one: a key set again moves to the end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvList(Vec<(Vec<u8>, Vec<u8>)>);

impl EnvList {
    pub const fn new() -> EnvList {
        EnvList(Vec::new())
    }

    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        self.0.retain(|(k, _)| k != key);
        self.0.push((key.to_vec(), value.to_vec()));
    }

    /// Adds each of `entries` in turn, as [`add`](Self::add) does, in time linear in
    /// them and the list: each key ends up where it is added last.
    pub fn extend<'a>(&mut self, entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) {
        let entries: Vec<(&[u8], &[u8])> = entries.into_iter().collect();
        let last: HashMap<&[u8], usize> = entries.iter().enumerate().map(|(i, &(k, _))| (k, i)).collect();
        self.0.retain(|(k, _)| !last.contains_key(k.as_slice()));
        for (i, &(k, v)) in entries.iter().enumerate() {
            if last.get(k) == Some(&i) {
                self.0.push((k.to_vec(), v.to_vec()));
            }
        }
    }

    pub fn delete(&mut self, key: &[u8]) {
        self.0.retain(|(k, _)| k != key);
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_slice())
    }

    pub fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.0.iter().map(|(k, _)| k.as_slice())
    }

    pub fn entries(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.0
    }

    /// `ToArray`: `key=value` each.
    pub fn to_array(&self) -> Vec<Vec<u8>> {
        self.0
            .iter()
            .map(|(k, v)| [k.as_slice(), b"=", v].concat())
            .collect()
    }
}

impl crate::lex::Env for EnvList {
    fn get(&self, key: &[u8]) -> Option<&[u8]> {
        EnvList::get(self, key)
    }
}

/// `llb.State`: an output, and what the next command runs with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub output: Option<Output>,
    pub env: EnvList,
    /// The working directory: `/` unless set.
    pub dir: Vec<u8>,
    pub user: Vec<u8>,
    pub hostname: Vec<u8>,
    pub network: NetMode,
    pub security: Security,
    pub platform: Option<Platform>,
    pub extra_hosts: Vec<HostIp>,
    pub ulimits: Vec<Ulimit>,
    pub cgroup_parent: Vec<u8>,
}

impl State {
    /// `llb.Scratch()`.
    pub fn scratch() -> State {
        State {
            dir: b"/".to_vec(),
            ..State::default()
        }
    }

    /// `State.Dir`: a relative directory joins the one before.
    pub fn set_dir(&mut self, dir: &[u8]) {
        self.dir = if go::is_abs(dir) {
            dir.to_vec()
        } else {
            let prev: &[u8] = if self.dir.is_empty() { b"/" } else { &self.dir };
            go::join(&[prev, dir])
        };
    }
}

/// The plan's vertices, and the source maps of the definitions given whole in it.
#[derive(Debug, Default)]
pub struct Graph {
    pub vertices: Vec<Vertex>,
    progress_groups: usize,
    pub sources: Vec<GivenSource>,
    /// Whether a definition was given whole into it, whose ops may be ones of its own.
    given: bool,
}

/// A command for `Graph::run`.
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub args: Vec<Vec<u8>>,
    pub mounts: Vec<Mount>,
    pub secrets: Vec<Secret>,
    pub ssh: Vec<Ssh>,
    pub devices: Vec<Device>,
    pub proxy: Option<ProxyEnv>,
    pub network: Option<NetMode>,
    pub security: Option<Security>,
    pub meta: Meta,
}

impl Graph {
    fn add(&mut self, v: Vertex) -> VertexId {
        self.vertices.push(v);
        self.vertices.len() - 1
    }

    /// A new progress group's ID.
    pub fn progress_group(&mut self) -> usize {
        self.progress_groups += 1;
        self.progress_groups - 1
    }

    /// `llb.Image` and the like: a source, as a state whose platform is the source's.
    pub fn source(
        &mut self,
        identifier: Vec<u8>,
        attrs: BTreeMap<Vec<u8>, Vec<u8>>,
        platform: Option<Platform>,
        meta: Meta,
    ) -> State {
        // Only images and OCI layouts are built for a platform (`platformSpecificSource`).
        let specific = identifier.starts_with(b"docker-image://") || identifier.starts_with(b"oci-layout://");
        let id = self.add(Vertex {
            kind: Kind::Source { identifier, attrs },
            platform: if specific { platform.clone() } else { None },
            meta,
        });
        let mut s = State::scratch();
        s.output = Some(Output { vertex: id, index: 0 });
        s.platform = platform.map(|p| crate::platform::normalize(&p));
        s
    }

    /// `State.Run(...).Root()`: the command on `state`, the state of its root after.
    pub fn run(&mut self, state: &State, run: Run) -> State {
        let mut mounts = vec![Mount {
            target: b"/".to_vec(),
            source: state.output,
            readonly: false,
            selector: Vec::new(),
            kind: MountKind::Bind,
            no_output: false,
        }];
        mounts.extend(run.mounts);
        // Sorted by target, as BuildKit sorts them; the root, added first, stays first
        // among equal targets.
        mounts.sort_by(|a, b| a.target.cmp(&b.target));
        // The root's output: its place among the mounts that have one.
        let mut index = 0;
        for m in &mounts {
            if m.target == b"/" && m.source == state.output && !m.readonly {
                break;
            }
            if has_output(m) {
                index += 1;
            }
        }
        let process = Process {
            args: run.args,
            env: state.env.to_array(),
            cwd: state.dir.clone(),
            user: state.user.clone(),
            proxy: run.proxy,
            extra_hosts: state.extra_hosts.clone(),
            hostname: state.hostname.clone(),
            ulimits: state.ulimits.clone(),
            cgroup_parent: state.cgroup_parent.clone(),
        };
        let id = self.add(Vertex {
            kind: Kind::Exec {
                process: Box::new(process),
                mounts,
                network: run.network.unwrap_or(state.network),
                security: run.security.unwrap_or(state.security),
                secrets: run.secrets,
                ssh: run.ssh,
                devices: run.devices,
            },
            platform: state.platform.clone(),
            meta: run.meta,
        });
        let mut s = state.clone();
        s.output = Some(Output { vertex: id, index });
        s
    }

    /// `State.File(actions)`: the actions, in order, on `state`.
    pub fn file(&mut self, state: &State, actions: Vec<Action>, meta: Meta) -> State {
        let id = self.add(Vertex {
            kind: Kind::File {
                base: state.output,
                base_dir: state.dir.clone(),
                actions,
            },
            platform: None,
            meta,
        });
        let mut s = state.clone();
        s.output = Some(Output { vertex: id, index: 0 });
        s
    }

    /// `llb.Merge`'s output: the outputs layered in order. Scratch adds nothing, and one
    /// output alone is itself.
    /// The skills `state` holds, laid out as [`Kind::Skills`] says.
    pub fn skills(&mut self, state: &State, name: &[u8], meta: Meta) -> State {
        let mut out = state.clone();
        out.output = state.output.map(|input| {
            let id = self.add(Vertex {
                kind: Kind::Skills {
                    input,
                    name: name.to_vec(),
                },
                platform: None,
                meta,
            });
            Output { vertex: id, index: 0 }
        });
        out
    }

    pub fn merge(&mut self, outputs: &[Option<Output>], meta: Meta) -> Option<Output> {
        let inputs: Vec<Output> = outputs.iter().flatten().copied().collect();
        match inputs.as_slice() {
            [] => None,
            [one] => Some(*one),
            _ => {
                let id = self.add(Vertex {
                    kind: Kind::Merge { inputs },
                    platform: None,
                    meta,
                });
                Some(Output { vertex: id, index: 0 })
            }
        }
    }
}

fn has_output(m: &Mount) -> bool {
    !m.no_output && !m.readonly && matches!(m.kind, MountKind::Bind)
}

/// An input of a marshalled operation: another operation's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Input {
    pub op: usize,
    pub index: i64,
}

/// A command's mount as `pb.Mount` has it: inputs and outputs by number, `-1` for none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpMount {
    pub input: i64,
    pub selector: Vec<u8>,
    pub dest: Vec<u8>,
    pub output: i64,
    pub readonly: bool,
    pub kind: OpMountKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpMountKind {
    Bind,
    Cache {
        id: Vec<u8>,
        sharing: Sharing,
    },
    Tmpfs {
        size: i64,
    },
    Secret {
        id: Vec<u8>,
        uid: u32,
        gid: u32,
        mode: u32,
        optional: bool,
    },
    Ssh {
        id: Vec<u8>,
        uid: u32,
        gid: u32,
        mode: u32,
        optional: bool,
    },
}

/// A file action as `pb.FileAction` has it. An input below the operation's count of
/// inputs is that input; at or above it, the output of the action that many past it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpAction {
    pub input: i64,
    pub secondary_input: i64,
    pub output: i64,
    pub action: OpActionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpActionKind {
    Mkdir {
        path: Vec<u8>,
        mode: i32,
        make_parents: bool,
        owner: Option<OpChown>,
        timestamp: i64,
    },
    Mkfile {
        path: Vec<u8>,
        mode: i32,
        data: Vec<u8>,
        owner: Option<OpChown>,
        timestamp: i64,
    },
    Copy {
        src: Vec<u8>,
        dest: Vec<u8>,
        owner: Option<OpChown>,
        mode: i32,
        mode_str: Vec<u8>,
        follow_symlink: bool,
        dir_copy_contents: bool,
        attempt_unpack: bool,
        create_dest_path: bool,
        allow_wildcard: bool,
        allow_empty_wildcard: bool,
        timestamp: i64,
        include_patterns: Vec<Vec<u8>>,
        exclude_patterns: Vec<Vec<u8>>,
        required_paths: Vec<Vec<u8>>,
    },
}

/// `pb.ChownOpt`: a name is looked up in the input numbered beside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpChown {
    pub user: Option<OpUser>,
    pub group: Option<OpUser>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpUser {
    Id(u32),
    Name { name: Vec<u8>, input: i64 },
}

/// A marshalled operation: `pb.Op`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Op {
    pub inputs: Vec<Input>,
    pub kind: OpKind,
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpKind {
    Source {
        identifier: Vec<u8>,
        attrs: BTreeMap<Vec<u8>, Vec<u8>>,
    },
    Exec {
        process: Box<Process>,
        mounts: Vec<OpMount>,
        network: NetMode,
        security: Security,
        secret_env: Vec<(Vec<u8>, Vec<u8>, bool)>,
        devices: Vec<Device>,
    },
    File {
        actions: Vec<OpAction>,
    },
    Merge,
    /// shards' own: see [`Kind::Skills`].
    Skills {
        name: Vec<u8>,
    },
    /// A [`Kind::Raw`]: its bytes as they came, and the source it is, if one.
    Raw {
        bytes: Vec<u8>,
        source: Option<RawSource>,
    },
}

/// `llb.Definition`: the operations a state needs, each once, every input before what
/// reads it; each one's metadata; and the state's own output. `sources` are the source
/// maps definitions given whole carry ([`Graph::sources`]); `source_order`, each map an
/// op is located in, in the order the marshal first meets it (`sourceMapCollector.Add`).
#[derive(Debug, Clone, Default)]
pub struct Definition {
    pub ops: Vec<Op>,
    pub metadata: Vec<Meta>,
    pub root: Option<Input>,
    pub sources: Vec<GivenSource>,
    pub source_order: Vec<SourceRef>,
}

impl Graph {
    /// `State.Marshal`: the definition of what `state` needs. A command with no platform
    /// of its own runs for `platform`, as the marshal's default constraint.
    pub fn marshal(&self, state: &State, platform: &Platform) -> Definition {
        let mut m = Marshal::new(self, platform);
        if let Some(out) = state.output {
            m.visit(out.vertex);
            m.def.root = m.input(out);
        }
        m.def
    }

    /// `llb.NewDefinitionOp`: the output `def`'s root names, a vertex of its bytes for
    /// each operation it needs and each output of it read (DefinitionOp's input cache),
    /// every one after what it reads; none for a definition of no operations, which is
    /// scratch. Refuses what the marshal of one refuses (`DefinitionOp.Validate`): an op
    /// of no metadata, a root read at a negative output; and what crashes BuildKit's,
    /// which reads an op that is not there, or an output at a negative index, before it
    /// validates: here the same refusals.
    pub fn embed(&mut self, def: &Embedded) -> Result<Option<Output>, String> {
        let Some(root) = def.ops.last() else {
            return Ok(None);
        };
        let Some(root_input) = root.inputs.first() else {
            return Err("invalid definition with no inputs on last vertex".into());
        };
        let by_digest: HashMap<Vec<u8>, &EmbeddedOp> =
            def.ops.iter().map(|o| (crate::pb::digest(&o.bytes), o)).collect();
        let base = self.sources.len();
        let mut made: HashMap<(Vec<u8>, i64), VertexId> = HashMap::new();
        let mut open: std::collections::HashSet<(Vec<u8>, i64)> = std::collections::HashSet::new();
        // Depth first, each read output before its reader, in the order the op reads them.
        let mut stack = vec![(root_input.clone(), false)];
        while let Some((key, ready)) = stack.pop() {
            if made.contains_key(&key) {
                continue;
            }
            let (digest, index) = &key;
            let op = by_digest
                .get(digest)
                .ok_or_else(|| format!("invalid definition op with unknown op {}", go::quote(digest)))?;
            if *index < 0 {
                return Err("invalid definition op with invalid index".into());
            }
            if !ready {
                if !open.insert(key.clone()) {
                    return Err("a definition whose operations read each other in a cycle".into());
                }
                stack.push((key, true));
                for input in op.inputs.iter().rev() {
                    if !made.contains_key(input) {
                        stack.push((input.clone(), false));
                    }
                }
                continue;
            }
            let mut meta = op
                .metadata
                .clone()
                .ok_or_else(|| format!("invalid definition op with unknown metas {}", go::quote(digest)))?;
            meta.given_locations = op
                .locations
                .iter()
                .map(|(source, ranges)| GivenLocation {
                    source: base + source,
                    ranges: ranges.clone(),
                    after: 0,
                })
                .collect();
            let inputs = op
                .inputs
                .iter()
                .map(|input| {
                    made.get(input).map(|&vertex| Output {
                        vertex,
                        index: input.1,
                    })
                })
                .collect::<Option<Vec<Output>>>()
                .ok_or("a definition whose operations read each other in a cycle")?;
            let id = self.add(Vertex {
                kind: Kind::Raw {
                    bytes: op.bytes.clone(),
                    inputs,
                    source: op.source.clone(),
                },
                platform: None,
                meta,
            });
            open.remove(&key);
            made.insert(key, id);
        }
        let vertex = made
            .get(root_input)
            .copied()
            .ok_or("invalid definition op with no root")?;
        self.sources.extend(def.sources.iter().cloned());
        self.given = true;
        Ok(Some(Output {
            vertex,
            index: root_input.1,
        }))
    }

    /// [`Graph::marshal`] of `state`, and after its operations those `also` needs, which
    /// the build runs beside it (an SBOM scan of its result, D81): its root is `state`'s,
    /// and each of `also`'s outputs is where it is in the definition.
    pub fn marshal_with(
        &self,
        state: &State,
        also: &[&State],
        platform: &Platform,
    ) -> (Definition, Vec<Option<Input>>) {
        let mut m = Marshal::new(self, platform);
        if let Some(out) = state.output {
            m.visit(out.vertex);
            m.def.root = m.input(out);
        }
        let mut found = Vec::with_capacity(also.len());
        for s in also {
            found.push(s.output.and_then(|out| {
                m.visit(out.vertex);
                m.input(out)
            }));
        }
        (m.def, found)
    }
}

struct Marshal<'g> {
    graph: &'g Graph,
    platform: &'g Platform,
    def: Definition,
    by_content: HashMap<Op, usize>,
    of_vertex: HashMap<VertexId, usize>,
    /// Where a definition was given whole: each op's digest, and the op of each, by
    /// which an op given and one of our own that are the same are one (`marshal`'s cache).
    digests: Vec<Vec<u8>>,
    by_digest: HashMap<Vec<u8>, usize>,
}

impl<'g> Marshal<'g> {
    fn new(graph: &'g Graph, platform: &'g Platform) -> Marshal<'g> {
        Marshal {
            graph,
            platform,
            def: Definition {
                sources: graph.sources.clone(),
                ..Definition::default()
            },
            by_content: HashMap::new(),
            of_vertex: HashMap::new(),
            digests: Vec::new(),
            by_digest: HashMap::new(),
        }
    }

    /// `op`'s digest, where a definition was given whole: its bytes, its inputs named by
    /// theirs.
    fn digest(&self, op: &Op) -> Option<Vec<u8>> {
        if !self.graph.given {
            return None;
        }
        let inputs: Vec<Vec<u8>> = op
            .inputs
            .iter()
            .map(|i| self.digests.get(i.op).filter(|d| !d.is_empty()).cloned())
            .collect::<Option<_>>()?;
        crate::pb::op(op, &inputs).map(|b| crate::pb::digest(&b))
    }

    /// sourceMapCollector.Add: the maps `meta`'s locations are in, each where first met.
    fn meet_sources(&mut self, meta: &Meta) {
        let mut meet = |r: SourceRef| {
            if !self.def.source_order.contains(&r) {
                self.def.source_order.push(r);
            }
        };
        if !meta.locations.is_empty() {
            meet(SourceRef::Own);
        }
        for g in &meta.given_locations {
            meet(SourceRef::Given(g.source));
        }
    }

    fn input(&self, o: Output) -> Option<Input> {
        self.of_vertex
            .get(&o.vertex)
            .map(|&op| Input { op, index: o.index })
    }

    /// Marshals `v` after what it reads, depth first, as BuildKit's `marshal` does,
    /// without recursion.
    fn visit(&mut self, root: VertexId) {
        // Each entry: a vertex, and whether its inputs are done.
        let mut stack = vec![(root, false)];
        while let Some((v, ready)) = stack.pop() {
            if self.of_vertex.contains_key(&v) {
                continue;
            }
            let Some(vertex) = self.graph.vertices.get(v) else {
                continue;
            };
            if !ready {
                stack.push((v, true));
                // Visited in order: the first pushed last.
                for i in vertex.inputs().iter().rev() {
                    if !self.of_vertex.contains_key(&i.vertex) {
                        stack.push((i.vertex, false));
                    }
                }
                continue;
            }
            let op = self.op(vertex);
            let digest = self.digest(&op);
            let found = self
                .by_content
                .get(&op)
                .or_else(|| digest.as_ref().and_then(|d| self.by_digest.get(d)))
                .copied();
            let index = match found {
                Some(i) => {
                    // An op given whole and one of our own that is the same: ours, whose
                    // kind says the capabilities it needs; both are its bytes.
                    if let Some(slot) = self.def.ops.get_mut(i)
                        && matches!(slot.kind, OpKind::Raw { .. })
                        && !matches!(op.kind, OpKind::Raw { .. })
                    {
                        *slot = op.clone();
                    }
                    self.by_content.entry(op).or_insert(i);
                    i
                }
                None => {
                    let i = self.def.ops.len();
                    self.def.ops.push(op.clone());
                    self.def.metadata.push(Meta::default());
                    self.by_content.insert(op, i);
                    if self.graph.given {
                        // One for every op, empty where none is known, so they line up.
                        self.digests.push(digest.clone().unwrap_or_default());
                    }
                    if let Some(d) = digest {
                        self.by_digest.insert(d, i);
                    }
                    i
                }
            };
            if let Some(md) = self.def.metadata.get_mut(index) {
                md.merge(&vertex.meta);
            }
            self.meet_sources(&vertex.meta);
            self.of_vertex.insert(v, index);
        }
    }

    /// The input list's index for `o`, added if new: inputs are equal when their
    /// operations and outputs are.
    fn add_input(&self, inputs: &mut Vec<Input>, o: Output) -> i64 {
        let Some(i) = self.input(o) else {
            return -1;
        };
        match inputs.iter().position(|x| *x == i) {
            Some(at) => at as i64,
            None => {
                inputs.push(i);
                inputs.len() as i64 - 1
            }
        }
    }

    fn op(&self, v: &Vertex) -> Op {
        let mut inputs = Vec::new();
        let kind = match &v.kind {
            Kind::Source { identifier, attrs } => OpKind::Source {
                identifier: identifier.clone(),
                attrs: attrs.clone(),
            },
            Kind::Exec {
                process,
                mounts,
                network,
                security,
                secrets,
                ssh,
                devices,
            } => {
                let mut out = Vec::new();
                let mut outputs = 0;
                for m in mounts {
                    let input = match m.source {
                        Some(s) => self.add_input(&mut inputs, s),
                        None => -1,
                    };
                    let output = if has_output(m) {
                        outputs += 1;
                        outputs - 1
                    } else {
                        -1
                    };
                    out.push(OpMount {
                        input,
                        selector: m.selector.clone(),
                        dest: m.target.clone(),
                        output,
                        readonly: m.readonly,
                        kind: match &m.kind {
                            MountKind::Bind => OpMountKind::Bind,
                            MountKind::Cache { id, sharing } => OpMountKind::Cache {
                                id: id.clone(),
                                sharing: *sharing,
                            },
                            MountKind::Tmpfs { size } => OpMountKind::Tmpfs { size: *size },
                        },
                    });
                }
                let mut secret_env = Vec::new();
                for s in secrets {
                    if let Some(env) = &s.env {
                        secret_env.push((s.id.clone(), env.clone(), s.optional));
                    }
                    if let Some(target) = &s.target {
                        out.push(OpMount {
                            input: -1,
                            selector: Vec::new(),
                            dest: target.clone(),
                            output: 0,
                            readonly: false,
                            kind: OpMountKind::Secret {
                                id: s.id.clone(),
                                uid: s.uid,
                                gid: s.gid,
                                mode: s.mode,
                                optional: s.optional,
                            },
                        });
                    }
                }
                // A socket without a target gets the agent's place by its index.
                let targets: Vec<Vec<u8>> = ssh
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        if s.target.is_empty() {
                            format!("/run/buildkit/ssh_agent.{i}").into_bytes()
                        } else {
                            s.target.clone()
                        }
                    })
                    .collect();
                for (s, target) in ssh.iter().zip(&targets) {
                    out.push(OpMount {
                        input: -1,
                        selector: Vec::new(),
                        dest: target.clone(),
                        output: 0,
                        readonly: false,
                        kind: OpMountKind::Ssh {
                            id: s.id.clone(),
                            uid: s.uid,
                            gid: s.gid,
                            mode: s.mode,
                            optional: s.optional,
                        },
                    });
                }
                let mut process = process.clone();
                // An SSH socket is the agent's unless the command names one.
                if let Some(first) = targets.first()
                    && !process.env.iter().any(|e| e.starts_with(b"SSH_AUTH_SOCK="))
                {
                    process.env.push([b"SSH_AUTH_SOCK=".as_slice(), first].concat());
                }
                OpKind::Exec {
                    process,
                    mounts: out,
                    network: *network,
                    security: *security,
                    secret_env,
                    devices: devices.clone(),
                }
            }
            Kind::File {
                base,
                base_dir,
                actions,
            } => {
                // Each action: its base, its input (the base for the first, the one
                // before's result after), and what it copies from.
                let mut staged = Vec::new();
                for (i, a) in actions.iter().enumerate() {
                    let b = base.map_or(-1, |o| self.add_input(&mut inputs, o));
                    let input = if i == 0 { Rel::Abs(b) } else { Rel::Action(i - 1) };
                    let secondary = match a {
                        Action::Copy { from, .. } => {
                            Rel::Abs(from.map_or(-1, |o| self.add_input(&mut inputs, o)))
                        }
                        _ => Rel::Abs(-1),
                    };
                    staged.push((b, input, secondary));
                }
                let n = inputs.len() as i64;
                let resolve = |r: &Rel| match r {
                    Rel::Abs(i) => *i,
                    Rel::Action(t) => n + *t as i64,
                };
                let last = actions.len().saturating_sub(1);
                let ops = actions
                    .iter()
                    .zip(&staged)
                    .enumerate()
                    .map(|(i, (a, (b, input, secondary)))| OpAction {
                        input: resolve(input),
                        secondary_input: resolve(secondary),
                        output: if i == last { 0 } else { -1 },
                        action: file_action(a, base_dir, *b),
                    })
                    .collect();
                OpKind::File { actions: ops }
            }
            Kind::Merge { inputs: ins } => {
                for o in ins {
                    // Every input its own, even when equal (`MergeOp.Marshal`).
                    if let Some(i) = self.input(*o) {
                        inputs.push(i);
                    }
                }
                OpKind::Merge
            }
            Kind::Skills { input, name } => {
                if let Some(i) = self.input(*input) {
                    inputs.push(i);
                }
                OpKind::Skills { name: name.clone() }
            }
            // Its inputs only order it after what it reads: its bytes name them.
            Kind::Raw {
                bytes,
                inputs: reads,
                source,
            } => {
                inputs.extend(reads.iter().filter_map(|o| self.input(*o)));
                OpKind::Raw {
                    bytes: bytes.clone(),
                    source: source.clone(),
                }
            }
        };
        // Commands and image sources are for a platform: theirs, or the default.
        let platform = match &kind {
            OpKind::Exec { .. } => Some(v.platform.clone().unwrap_or_else(|| self.platform.clone())),
            _ => v.platform.clone(),
        };
        Op {
            inputs,
            kind,
            platform,
        }
    }
}

enum Rel {
    Abs(i64),
    Action(usize),
}

/// `normalizePath`: `p` cleaned and made absolute under `parent`, a trailing `/` or `/.`
/// kept if asked.
fn normalize_path(parent: &[u8], p: &[u8], keep_slash: bool) -> Vec<u8> {
    let mut out = go::clean(p);
    if !go::is_abs(&out) {
        out = go::join(&[b"/", parent, &out]);
    }
    if keep_slash {
        if p.ends_with(b"/") && !out.ends_with(b"/") {
            out.push(b'/');
        } else if p.ends_with(b"/.") {
            if out != b"/" {
                out.push(b'/');
            }
            out.push(b'.');
        }
    }
    out
}

fn chown(c: &Option<Chown>, base: i64) -> Option<OpChown> {
    let user = |u: &Option<UserOpt>| {
        u.as_ref().map(|u| match u {
            UserOpt::Id(id) => OpUser::Id(*id),
            // An empty name is no name: Go's UserOpt marshals it as ID 0.
            UserOpt::Name(n) if n.is_empty() => OpUser::Id(0),
            UserOpt::Name(n) => OpUser::Name {
                name: n.clone(),
                input: base,
            },
        })
    };
    c.as_ref().map(|c| OpChown {
        user: user(&c.user),
        group: user(&c.group),
    })
}

fn file_action(a: &Action, base_dir: &[u8], base: i64) -> OpActionKind {
    let time = |t: &Option<i64>| t.unwrap_or(-1);
    match a {
        Action::Mkdir {
            path,
            mode,
            make_parents,
            chown: c,
            created,
        } => OpActionKind::Mkdir {
            path: normalize_path(base_dir, path, false),
            mode: (*mode & 0o777) as i32,
            make_parents: *make_parents,
            owner: chown(c, base),
            timestamp: time(created),
        },
        Action::Mkfile {
            path,
            mode,
            data,
            chown: c,
            created,
        } => OpActionKind::Mkfile {
            path: normalize_path(base_dir, path, false),
            mode: (*mode & 0o777) as i32,
            data: data.clone(),
            owner: chown(c, base),
            timestamp: time(created),
        },
        Action::Copy {
            from_dir,
            src,
            dest,
            info,
            ..
        } => {
            // sourcePath: cleaned, and under the source's directory if relative.
            let p = go::clean(src);
            let src = if go::is_abs(&p) {
                go::join(&[b"/", &p])
            } else {
                go::join(&[from_dir, &p])
            };
            let (mode, mode_str) = match &info.mode {
                None => (-1, Vec::new()),
                Some(Chmod::Mode(m)) => (*m as i32, Vec::new()),
                Some(Chmod::Str(s)) => (0, s.clone()),
            };
            OpActionKind::Copy {
                src,
                dest: normalize_path(base_dir, dest, true),
                owner: chown(&info.chown, base),
                mode,
                mode_str,
                follow_symlink: info.follow_symlinks,
                dir_copy_contents: info.dir_contents_only,
                attempt_unpack: info.attempt_unpack,
                create_dest_path: info.create_dest_path,
                allow_wildcard: info.allow_wildcard,
                allow_empty_wildcard: info.allow_empty_wildcard,
                timestamp: time(&info.created),
                include_patterns: info.include_patterns.clone(),
                exclude_patterns: info.exclude_patterns.clone(),
                required_paths: info.required_paths.clone(),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A message's fields, each its number and its value's bytes (a varint's as written).
    fn fields(b: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let varint = |b: &[u8], at: &mut usize| {
            let (mut v, mut shift) = (0u64, 0);
            loop {
                let c = b[*at];
                *at += 1;
                v |= u64::from(c & 0x7f) << shift;
                shift += 7;
                if c < 0x80 {
                    return v;
                }
            }
        };
        let (mut out, mut at) = (Vec::new(), 0);
        while at < b.len() {
            let tag = varint(b, &mut at);
            let start = at;
            let value = if tag & 7 == 0 {
                varint(b, &mut at);
                b[start..at].to_vec()
            } else {
                let n = varint(b, &mut at) as usize;
                at += n;
                b[at - n..at].to_vec()
            };
            out.push((tag >> 3, value));
        }
        out
    }

    fn values(b: &[u8], field: u64) -> Vec<Vec<u8>> {
        fields(b)
            .into_iter()
            .filter(|(f, _)| *f == field)
            .map(|(_, v)| v)
            .collect()
    }

    /// A map's entries, by key.
    fn entries(b: &[u8], field: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
        values(b, field)
            .iter()
            .map(|e| (values(e, 1).concat(), values(e, 2).concat()))
            .collect()
    }

    fn source_op(id: &[u8]) -> Op {
        Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: id.to_vec(),
                attrs: BTreeMap::new(),
            },
            platform: None,
        }
    }

    fn named(name: &[u8]) -> Meta {
        let mut m = Meta::default();
        m.description.insert(b"llb.customname".to_vec(), name.to_vec());
        m
    }

    fn lines(a: i32, b: i32) -> Vec<Range> {
        let at = |line| Some(Position { line, character: 0 });
        vec![Range {
            start: at(a),
            end: at(b),
        }]
    }

    fn mkfile(path: &[u8]) -> Action {
        Action::Mkfile {
            path: path.to_vec(),
            mode: 0o644,
            data: b"x".to_vec(),
            chown: None,
            created: None,
        }
    }

    /// A definition given whole (`llb.NewDefinitionOp`): the state is what its root names,
    /// each operation it needs written again as it came, so that its digest and those its
    /// readers name stay; one it holds that nothing needs left out; each one's metadata
    /// written as `client/llb` writes it again, its capabilities and all; its source map
    /// where the marshal first meets it, before the Dockerfile's here, and its locations
    /// naming it.
    #[test]
    fn a_definition_given_whole_is_written_as_it_came() {
        let src = crate::pb::op(&source_op(b"local://context"), &[]).unwrap();
        let unused = crate::pb::op(&source_op(b"local://other"), &[]).unwrap();
        let src_digest = crate::pb::digest(&src);
        let mut src_meta = named(b"[base 1/1] load");
        src_meta.caps.insert(b"source.local".to_vec());
        src_meta.progress_group = Some(ProgressGroup {
            id: GroupId::Given(b"given-group".to_vec()),
            name: b"loading".to_vec(),
            weak: false,
        });
        let given = Embedded {
            ops: vec![
                EmbeddedOp {
                    bytes: src.clone(),
                    metadata: Some(src_meta),
                    locations: vec![(0, lines(1, 1))],
                    ..EmbeddedOp::default()
                },
                EmbeddedOp {
                    bytes: unused.clone(),
                    metadata: Some(Meta::default()),
                    ..EmbeddedOp::default()
                },
                EmbeddedOp {
                    bytes: crate::pb::root(&src_digest, 0),
                    inputs: vec![(src_digest.clone(), 0)],
                    metadata: Some(Meta::default()),
                    ..EmbeddedOp::default()
                },
            ],
            sources: vec![GivenSource {
                filename: b"Dockerfile".to_vec(),
                data: b"FROM scratch AS base\n".to_vec(),
                language: b"Dockerfile".to_vec(),
                definition: Some((b"its-definition".to_vec(), "{}".to_string())),
            }],
        };
        let mut g = Graph::default();
        let mut st = State::scratch();
        st.output = g.embed(&given).unwrap();
        let mut own = named(b"[app 1/1] RUN");
        own.locations.push(vec![(2, 2)]);
        let made = g.file(&st, vec![mkfile(b"/x")], own);
        let def = g.marshal(&made, &Platform::new("linux", "arm64"));
        let ours = b"FROM base\nRUN x\n";
        let carried = crate::pb::Carried {
            source: Some(crate::pb::SourceInfo {
                filename: b"Dockerfile",
                language: b"Dockerfile",
                data: ours,
                definition: Some(b"our-definition"),
            }),
            ..crate::pb::Carried::default()
        };
        let m = crate::pb::definition(&def, &carried).unwrap();
        assert_eq!(m.digests.len(), 2, "the op nothing needs is left out");
        assert_eq!(m.digests[0], src_digest);
        let ops = values(&m.bytes, 1);
        assert_eq!(ops[0], src);
        assert!(!ops.contains(&unused));
        let metadata = entries(&m.bytes, 2);
        let src_md = &metadata[&src_digest];
        assert_eq!(
            entries(src_md, 2)[b"llb.customname".as_slice()],
            b"[base 1/1] load"
        );
        assert_eq!(entries(src_md, 5).keys().collect::<Vec<_>>(), [b"source.local"]);
        assert_eq!(values(&values(src_md, 6)[0], 1), [b"given-group"]);
        assert!(entries(&metadata[&m.root], 5).contains_key(b"meta.description".as_slice()));
        assert!(m.caps.contains("source.local"));
        // The given map first, met first; the Dockerfile's second.
        let source = &values(&m.bytes, 3)[0];
        let infos = values(source, 2);
        assert_eq!(infos.len(), 2);
        assert_eq!(values(&infos[0], 2), [b"FROM scratch AS base\n"]);
        assert_eq!(values(&infos[0], 3), [b"its-definition"]);
        assert_eq!(values(&infos[1], 2), [ours]);
        let locations = entries(source, 1);
        let src_loc = &values(&locations[&src_digest], 1)[0];
        assert!(values(src_loc, 1).is_empty(), "index 0, left out");
        let file_loc = &values(&locations[&m.digests[1]], 1)[0];
        assert_eq!(values(file_loc, 1), [vec![1]]);
        // No operations: scratch.
        assert_eq!(g.embed(&Embedded::default()), Ok(None));
    }

    /// A map given whole and the Dockerfile's that are alike (file, language and data)
    /// are one (`equalSourceMap`): the first met kept, every location naming it.
    #[test]
    fn source_maps_alike_are_one() {
        let src = crate::pb::op(&source_op(b"local://context"), &[]).unwrap();
        let src_digest = crate::pb::digest(&src);
        let text = b"FROM scratch AS base\nFROM base\n";
        let given = Embedded {
            ops: vec![
                EmbeddedOp {
                    bytes: src.clone(),
                    metadata: Some(Meta::default()),
                    locations: vec![(0, lines(1, 1))],
                    ..EmbeddedOp::default()
                },
                EmbeddedOp {
                    bytes: crate::pb::root(&src_digest, 0),
                    inputs: vec![(src_digest.clone(), 0)],
                    metadata: Some(Meta::default()),
                    ..EmbeddedOp::default()
                },
            ],
            sources: vec![GivenSource {
                filename: b"Dockerfile".to_vec(),
                data: text.to_vec(),
                language: b"Dockerfile".to_vec(),
                definition: None,
            }],
        };
        let mut g = Graph::default();
        let mut st = State::scratch();
        st.output = g.embed(&given).unwrap();
        let mut own = Meta::default();
        own.locations.push(vec![(2, 2)]);
        let made = g.file(&st, vec![mkfile(b"/x")], own);
        let def = g.marshal(&made, &Platform::new("linux", "arm64"));
        let carried = crate::pb::Carried {
            source: Some(crate::pb::SourceInfo {
                filename: b"Dockerfile",
                language: b"Dockerfile",
                data: text,
                definition: Some(b"ours"),
            }),
            ..crate::pb::Carried::default()
        };
        let m = crate::pb::definition(&def, &carried).unwrap();
        let source = &values(&m.bytes, 3)[0];
        let infos = values(source, 2);
        assert_eq!(infos.len(), 1);
        assert!(
            values(&infos[0], 3).is_empty(),
            "the given one, which came with none"
        );
        let locations = entries(source, 1);
        for d in &m.digests {
            assert!(values(&values(&locations[d], 1)[0], 1).is_empty());
        }
    }

    /// An op given whole and one of our own that is the same are one op (`marshal`'s
    /// cache by digest), its metadata merged: the descriptions, the later over the
    /// earlier; the capabilities its kind needs and those given.
    #[test]
    fn an_op_given_and_one_of_ours_alike_are_one() {
        let src = crate::pb::op(&source_op(b"local://context"), &[]).unwrap();
        let src_digest = crate::pb::digest(&src);
        let mut given_meta = named(b"given");
        given_meta.caps.insert(b"given.cap".to_vec());
        given_meta.export_cache = Some(true);
        let given = Embedded {
            ops: vec![
                EmbeddedOp {
                    bytes: src.clone(),
                    metadata: Some(given_meta),
                    ..EmbeddedOp::default()
                },
                EmbeddedOp {
                    bytes: crate::pb::root(&src_digest, 0),
                    inputs: vec![(src_digest.clone(), 0)],
                    metadata: Some(Meta::default()),
                    ..EmbeddedOp::default()
                },
            ],
            sources: Vec::new(),
        };
        let mut g = Graph::default();
        let given_out = g.embed(&given).unwrap();
        let own = g.source(b"local://context".to_vec(), BTreeMap::new(), None, named(b"ours"));
        let merged = g.merge(&[given_out, own.output], Meta::default());
        let st = State {
            output: merged,
            ..State::scratch()
        };
        let def = g.marshal(&st, &Platform::new("linux", "arm64"));
        assert_eq!(def.ops.len(), 2, "the source once, and the merge");
        assert!(
            matches!(def.ops[0].kind, OpKind::Source { .. }),
            "ours, whose kind is known"
        );
        let m = crate::pb::definition(&def, &crate::pb::Carried::default()).unwrap();
        assert_eq!(m.digests[0], src_digest);
        let metadata = entries(&m.bytes, 2);
        let md = &metadata[&src_digest];
        assert_eq!(entries(md, 2)[b"llb.customname".as_slice()], b"ours");
        let caps = entries(md, 5);
        assert!(caps.contains_key(b"given.cap".as_slice()) && caps.contains_key(b"source.local".as_slice()));
        assert_eq!(values(md, 4), [vec![8, 1]], "export_cache, its Value true");
        assert!(entries(&metadata[&m.root], 5).contains_key(b"meta.exportcache".as_slice()));
    }

    /// Each output of an op given whole that is read is a vertex of its own
    /// (DefinitionOp's input cache), each adding the op's locations again.
    #[test]
    fn each_output_read_adds_its_locations() {
        let a = crate::pb::op(&source_op(b"local://a"), &[]).unwrap();
        let a_digest = crate::pb::digest(&a);
        let reader = Op {
            inputs: vec![Input { op: 0, index: 0 }, Input { op: 0, index: 1 }],
            kind: OpKind::Merge,
            platform: None,
        };
        let b = crate::pb::op(&reader, &[a_digest.clone(), a_digest.clone()]).unwrap();
        let b_digest = crate::pb::digest(&b);
        let given = Embedded {
            ops: vec![
                EmbeddedOp {
                    bytes: a,
                    metadata: Some(Meta::default()),
                    locations: vec![(0, lines(3, 3))],
                    ..EmbeddedOp::default()
                },
                EmbeddedOp {
                    bytes: b,
                    inputs: vec![(a_digest.clone(), 0), (a_digest.clone(), 1)],
                    metadata: Some(Meta::default()),
                    ..EmbeddedOp::default()
                },
                EmbeddedOp {
                    bytes: crate::pb::root(&b_digest, 0),
                    inputs: vec![(b_digest.clone(), 0)],
                    metadata: Some(Meta::default()),
                    ..EmbeddedOp::default()
                },
            ],
            sources: vec![GivenSource::default()],
        };
        let mut g = Graph::default();
        let st = State {
            output: g.embed(&given).unwrap(),
            ..State::scratch()
        };
        let def = g.marshal(&st, &Platform::new("linux", "arm64"));
        assert_eq!(def.ops.len(), 2);
        assert_eq!(def.metadata[0].given_locations.len(), 2);
    }

    /// What BuildKit's marshal of a definition given whole refuses, refused when it is
    /// given: a root that reads nothing, an op of no metadata, a root at a negative
    /// output; and, where BuildKit's crashes, an op it reads that is not there.
    #[test]
    fn a_broken_definition_given_whole_is_refused() {
        let src = crate::pb::op(&source_op(b"local://context"), &[]).unwrap();
        let src_digest = crate::pb::digest(&src);
        let with = |metadata: Option<Meta>, index: i64, read: &[u8]| Embedded {
            ops: vec![
                EmbeddedOp {
                    bytes: src.clone(),
                    metadata,
                    ..EmbeddedOp::default()
                },
                EmbeddedOp {
                    bytes: crate::pb::root(read, index),
                    inputs: vec![(read.to_vec(), index)],
                    metadata: Some(Meta::default()),
                    ..EmbeddedOp::default()
                },
            ],
            sources: Vec::new(),
        };
        let mut g = Graph::default();
        assert!(g.embed(&with(Some(Meta::default()), 0, &src_digest)).is_ok());
        let no_input = Embedded {
            ops: vec![EmbeddedOp::default()],
            sources: Vec::new(),
        };
        assert_eq!(
            g.embed(&no_input),
            Err("invalid definition with no inputs on last vertex".to_string())
        );
        let quoted = go::quote(&src_digest);
        assert_eq!(
            g.embed(&with(None, 0, &src_digest)),
            Err(format!("invalid definition op with unknown metas {quoted}"))
        );
        assert_eq!(
            g.embed(&with(Some(Meta::default()), -1, &src_digest)),
            Err("invalid definition op with invalid index".to_string())
        );
        assert_eq!(
            g.embed(&with(Some(Meta::default()), 0, b"sha256:00")),
            Err(format!(
                "invalid definition op with unknown op {}",
                go::quote(b"sha256:00")
            ))
        );
    }

    /// Adding entries together leaves what adding them one by one does: keys already
    /// there and keys given twice end up where they are added last.
    #[test]
    fn entries_added_together_land_as_added_one_by_one() {
        let start = [(&b"A"[..], &b"1"[..]), (b"B", b"2"), (b"C", b"3")];
        let added = [
            (&b"B"[..], &b"x"[..]),
            (b"D", b"4"),
            (b"B", b"y"),
            (b"A", b"z"),
            (b"D", b"5"),
        ];
        let mut one_by_one = EnvList::new();
        let mut together = EnvList::new();
        for (k, v) in start {
            one_by_one.add(k, v);
            together.add(k, v);
        }
        for (k, v) in added {
            one_by_one.add(k, v);
        }
        together.extend(added);
        assert_eq!(together, one_by_one);
        assert_eq!(
            together.to_array(),
            [b"C=3".to_vec(), b"B=y".to_vec(), b"A=z".to_vec(), b"D=5".to_vec()]
        );
    }
}
