//! A build's plan as BuildKit's LLB holds it (`client/llb`, `solver/pb` of moby/buildkit
//! dockerfile/1.27.1): states that carry an output and the environment, directory, user
//! and platform the next step runs with; vertices (image, context and other sources,
//! commands, file operations, merges) built from them; and the definition that marshals
//! from a state, each operation once however many vertices make it, in an order where
//! every input comes first, with its metadata merged as BuildKit merges it.
//!
//! Strings are Go strings, bytes.

use std::collections::{BTreeMap, HashMap};

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
    pub progress_group: Option<ProgressGroup>,
    pub linux_resources: Option<LinuxResources>,
    /// Where in the Dockerfile it comes from (`llb.SourceMap.Location`): each a set of
    /// lines, of every vertex marshalled to the op, in the order they were (D80).
    pub locations: Vec<crate::instructions::Location>,
}

impl Meta {
    /// `mergeMetadata`: `other` over `self`.
    fn merge(&mut self, other: &Meta) {
        self.ignore_cache |= other.ignore_cache;
        for (k, v) in &other.description {
            self.description.insert(k.clone(), v.clone());
        }
        if other.progress_group.is_some() {
            self.progress_group.clone_from(&other.progress_group);
        }
        if other.linux_resources.is_some() {
            self.linux_resources.clone_from(&other.linux_resources);
        }
        // sourceMapCollector.Add: each vertex's locations, appended.
        self.locations.extend(other.locations.iter().cloned());
    }
}

/// `pb.ProgressGroup`. Its ID is the group's place among the plan's groups, where
/// BuildKit draws a random one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressGroup {
    pub id: usize,
    pub name: Vec<u8>,
    pub weak: bool,
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

/// The plan's vertices.
#[derive(Debug, Default)]
pub struct Graph {
    pub vertices: Vec<Vertex>,
    progress_groups: usize,
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
}

/// `llb.Definition`: the operations a state needs, each once, every input before what
/// reads it; each one's metadata; and the state's own output.
#[derive(Debug, Clone, Default)]
pub struct Definition {
    pub ops: Vec<Op>,
    pub metadata: Vec<Meta>,
    pub root: Option<Input>,
}

impl Graph {
    /// `State.Marshal`: the definition of what `state` needs. A command with no platform
    /// of its own runs for `platform`, as the marshal's default constraint.
    pub fn marshal(&self, state: &State, platform: &Platform) -> Definition {
        let mut m = Marshal {
            graph: self,
            platform,
            def: Definition::default(),
            by_content: HashMap::new(),
            of_vertex: HashMap::new(),
        };
        if let Some(out) = state.output {
            m.visit(out.vertex);
            m.def.root = m.input(out);
        }
        m.def
    }
}

struct Marshal<'g> {
    graph: &'g Graph,
    platform: &'g Platform,
    def: Definition,
    by_content: HashMap<Op, usize>,
    of_vertex: HashMap<VertexId, usize>,
}

impl Marshal<'_> {
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
            let index = match self.by_content.get(&op) {
                Some(&i) => i,
                None => {
                    self.def.ops.push(op.clone());
                    self.def.metadata.push(Meta::default());
                    self.by_content.insert(op, self.def.ops.len() - 1);
                    self.def.ops.len() - 1
                }
            };
            if let Some(md) = self.def.metadata.get_mut(index) {
                md.merge(&vertex.meta);
            }
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
