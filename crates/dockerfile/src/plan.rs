//! A Dockerfile planned as BuildKit's `dockerfile2llb.Dockerfile2LLB` plans it
//! (moby/buildkit dockerfile/1.27.1): build stages resolved against their base images,
//! `ARG` and `ENV` scoped as BuildKit scopes them, each step turned into the LLB vertex
//! BuildKit makes of it (its command, environment, directory, user, mounts, file
//! operations, and progress name such as `[stage-1 2/5] RUN ...`), the image config and
//! history it writes, and the build checks' warnings.
//!
//! Where shards does better, deliberately:
//! - nothing depends on map order: suggestions, undefined variables and the rules an
//!   `error=true` check names come in a fixed order;
//! - the build context's unique ID and progress groups' IDs are given, not random;
//! - targets are Linux, the only guests a microVM runs: signals are Linux's, and a
//!   Windows target is refused rather than planned;
//! - planning does no network I/O: an `ADD` of a git repository over SSH carries no
//!   host keys scanned while planning (`git.knownsshhosts`); the fetch verifies them.

use std::collections::{BTreeMap, BTreeSet};

use shards_image::reference::Reference;

use crate::git;
use crate::go;
use crate::image::{History, Image};
use crate::instructions::{self, ArgDef, Command, Kind, Location, Stage};
use crate::lex::{self, Lex};
use crate::lint::{self, Linter, LinterView};
use crate::llb::{
    self, Action, Chmod, Chown, CopyInfo, EnvList, Graph, Meta, Mount, MountKind, NetMode, Output,
    ProgressGroup, Secret, Security, Sharing, Ssh, State,
};
use crate::parser;
use crate::platform::{self, Platform};
use crate::subrequests::{self, Arg, Outline, Target, Targets};
use crate::url;

/// What the build is asked: dockerui's `Config` as Dockerfile2LLB reads it.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The platform the image is built for.
    pub target_platform: Platform,
    /// The platforms the build itself runs on; the first is `BUILDPLATFORM`.
    pub build_platforms: Vec<Platform>,
    pub build_args: BTreeMap<Vec<u8>, Vec<u8>>,
    /// The stage to build; the last when empty.
    pub target: Vec<u8>,
    /// Whether every stage is planned, reached from the target or not, as the lint
    /// subrequest plans a file with no target (`AllStages`).
    pub all_stages: bool,
    pub labels: BTreeMap<Vec<u8>, Vec<u8>>,
    pub hostname: Vec<u8>,
    /// The ulimits every RUN takes (the frontend's `ulimit` option, `--ulimit`'s).
    pub ulimits: Vec<llb::Ulimit>,
    /// Whether progress names carry the platform, as for a multi-platform build.
    pub multi_platform: bool,
    /// The build context's `local.unique`.
    pub context_id: Vec<u8>,
    /// The `.dockerignore` patterns the context is sent without (`local.excludepatterns`,
    /// as dockerui's MainContext sets them).
    pub excludes: Vec<Vec<u8>>,
    /// What the file is read as: a Dockerfile, or an Agentfile (D35).
    pub dialect: parser::Dialect,
    /// The named contexts (`--build-context`): the frontend's `context:KEY` options, each
    /// KEY a name or `NAME::PLATFORM`, each value `docker-image://`, `local:`, a Git or
    /// HTTP URL, as buildx sends them.
    pub contexts: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Each local named context's `sharedkey:localdir:KEY` option, by KEY.
    pub context_keys: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Each local named context's `.dockerignore` patterns, by its local name.
    pub context_excludes: BTreeMap<Vec<u8>, Vec<Vec<u8>>>,
    /// The frontend's `no-cache` option (dockerui's `ignoreCache`): the stages whose steps
    /// the cache is not asked for, every stage where it is empty, none where it is absent.
    pub no_cache: Option<Vec<Vec<u8>>>,
    /// Each RUN's, as the frontend's options give them (dockerui.rs): hosts added to its
    /// `/etc/hosts` (`add-hosts`), its `/dev/shm`'s size (`shm-size`, none if 0), its
    /// cgroup's parent (`cgroup-parent`) and its limits (`memory` and the like).
    pub extra_hosts: Vec<llb::HostIp>,
    pub shm_size: i64,
    pub cgroup_parent: Vec<u8>,
    pub linux_resources: Option<llb::LinuxResources>,
    /// Each stage's network unless a RUN says otherwise (`force-network-mode`).
    pub network_mode: NetMode,
    /// How a stage's base image is resolved (`image-resolve-mode`, dockerui.rs): its image
    /// source's `image.resolvemode`, none for the default.
    pub image_resolve_mode: Vec<u8>,
}

/// A named context a stage or base name is given (dockerui's NamedContext): its name (the
/// reference's familiar form), the key it was found by, and its value.
#[derive(Debug, Clone)]
struct Named {
    name: Vec<u8>,
    key: Vec<u8>,
    value: Vec<u8>,
}

/// A base image as resolved: its reference, digest and config.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub reference: Vec<u8>,
    pub digest: Option<Vec<u8>>,
    pub config: Vec<u8>,
}

/// Finds a base image's config: `ResolveImageConfig`; and when a source SOURCE_DATE_EPOCH
/// names was made: `resolveSourceDateEpochFromState`.
pub trait Resolver {
    /// `log` is what the step that resolves it is named, as BuildKit names it.
    fn resolve(&self, name: &[u8], platform: &Platform, log: &[u8]) -> Result<Resolved, Vec<u8>>;
    /// When `source` says it was made, if it says: seconds and nanoseconds since 1970.
    fn epoch(&self, source: &EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>>;
    /// An OSI artifact of `kind` (`agent`, `harness` or `mcp`, D54): its reference, with
    /// the digest it resolved to, and its config's bytes. `log` names the step.
    fn artifact(&self, name: &[u8], kind: &[u8], log: &[u8]) -> Result<Resolved, Vec<u8>> {
        let _ = (kind, log);
        Err(errb(&[name, b": OSI artifacts are not resolved here"]))
    }
}

/// Where SOURCE_DATE_EPOCH's time comes from when it is no number of seconds
/// (dockerfile/1.27.1 epoch.go): the build context, or the one remote ADD of a stage
/// that does nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpochSource {
    /// `context`.
    Context,
    /// A URL: its checksum, and the name its file takes (`sourceDateEpochHTTPFilename`).
    Http {
        stage: Vec<u8>,
        url: Vec<u8>,
        checksum: Option<Vec<u8>>,
        filename: Vec<u8>,
    },
    /// A Git repository, its checksum the ADD's `--checksum` when it has one.
    Git { stage: Vec<u8>, git: git::GitRef },
}

/// The metadata of a step that may write in no domain but its own (D55): every file
/// operation and command of the target's lineage carries it.
pub const GUARD: &[u8] = b"vnd.osi.guard";
/// The metadata of a domain's own directive's step: the destination it writes, whose
/// domain it may write.
pub const OWN: &[u8] = b"vnd.osi.own";

/// An agent's or harness's domain (AGENTFILE_ARCH.md §9.1): its name, its kind, and its
/// directory, absolute; its grants are in the directory beside it named `<dir>.d`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainDir {
    pub name: Vec<u8>,
    pub harness: bool,
    pub dir: Vec<u8>,
}

/// A planned build: its graph, the target's state, the image config and the warnings.
#[derive(Debug)]
pub struct Plan {
    pub graph: Graph,
    pub state: State,
    pub image: Image,
    pub platform: Platform,
    pub warnings: Vec<lint::Warning>,
    /// SOURCE_DATE_EPOCH, in seconds, when the build has one.
    pub epoch: Option<i64>,
    /// The domains the target's agents and harnesses make, for the export's checks.
    pub domains: Vec<DomainDir>,
    /// What an SBOM scanner is given beside the target (`SBOMTargets.Extras`), by name:
    /// the build context where the target's `BUILDKIT_SBOM_SCAN_CONTEXT` says so, and each
    /// stage it reaches whose `BUILDKIT_SBOM_SCAN_STAGE` names it (D81).
    pub sbom_extras: Vec<(Vec<u8>, State)>,
}

impl Plan {
    /// The definition to build: every operation the target needs.
    pub fn definition(&self) -> llb::Definition {
        self.graph.marshal(&self.state, &self.platform)
    }
}

/// An error and the lines it concerns: BuildKit's `LocationError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: Vec<u8>,
    pub location: Vec<Location>,
    /// The checks' warnings up to the error.
    pub warnings: Vec<lint::Warning>,
}

const EMPTY_IMAGE: &[u8] = b"scratch";
const HISTORY_COMMENT: &[u8] = b"buildkit.dockerfile.v0";
const DEFAULT_PATH: &[u8] = b"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const SBOM_SCAN_CONTEXT: &[u8] = b"BUILDKIT_SBOM_SCAN_CONTEXT";
const SBOM_SCAN_STAGE: &[u8] = b"BUILDKIT_SBOM_SCAN_STAGE";

/// `isEnabledForStage`: a boolean for every stage, else a list of the stages' names.
fn enabled_for_stage(stage: &[u8], value: &[u8]) -> bool {
    match go::parse_bool(value) {
        Some(b) => b,
        None => value.split(|&c| c == b',').any(|s| s == stage),
    }
}

/// Arguments that set no variable in a step's environment.
fn non_env_arg(key: &[u8]) -> bool {
    key == SBOM_SCAN_CONTEXT || key == SBOM_SCAN_STAGE
}

fn errb(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// An error with no location yet.
struct Fail(Vec<u8>, Vec<Location>);

impl Fail {
    fn new(m: impl Into<Vec<u8>>) -> Fail {
        Fail(m.into(), Vec::new())
    }

    /// `parser.WithLocation`: adds the location to those it has.
    fn at(mut self, loc: &Location) -> Fail {
        self.1.push(loc.clone());
        self
    }

    /// `parser.SetLocation`: replaces it.
    fn set_at(mut self, loc: &Location) -> Fail {
        self.1 = vec![loc.clone()];
        self
    }
}

impl From<Vec<u8>> for Fail {
    fn from(m: Vec<u8>) -> Fail {
        Fail::new(m)
    }
}

impl From<lex::Error> for Fail {
    fn from(e: lex::Error) -> Fail {
        Fail::new(e.0)
    }
}

/// A step of a stage, with the stages it reads from.
#[derive(Debug, Clone)]
struct Step {
    command: Command,
    /// For `COPY --from`, the stage; for `RUN`, one per mount.
    sources: Vec<usize>,
    on_build: bool,
}

/// `instructionTracker`: where an instruction was last used in the stage.
#[derive(Debug, Clone, Default)]
struct Tracker(Option<Location>);

/// `dispatchState`.
#[derive(Debug, Clone)]
struct Ds {
    state: State,
    image: Image,
    platform: Option<Platform>,
    stage: Stage,
    base: Option<usize>,
    dispatched: bool,
    resolved: bool,
    on_build_init: bool,
    /// The stages it reads, each with the location of the step that does.
    deps: Vec<(usize, Location)>,
    build_args: Vec<ArgDef>,
    steps: Vec<Step>,
    ctx_paths: BTreeSet<Vec<u8>>,
    /// Its paths' set: shared with the stage it builds on.
    paths: usize,
    unregistered: bool,
    stage_name: Vec<u8>,
    cmd_index: usize,
    cmd_is_on_build: bool,
    cmd_total: usize,
    workdir_set: bool,
    /// SOURCE_DATE_EPOCH: seconds and nanoseconds.
    epoch: Option<(i64, u32)>,
    entrypoint: Tracker,
    cmd: Tracker,
    healthcheck: Tracker,
    /// An Agentfile's directives in its lineage, in order (D35).
    agentfile: Vec<crate::agentfile::Directive>,
    /// The named context the stage itself is (its name is a context's).
    named: Option<Named>,
    /// The agents' and harnesses' domains in its lineage.
    domains: Vec<DomainDir>,
    /// Whether the cache is not asked for its steps (`ignoreCache`, `IsNoCache`).
    ignore_cache: bool,
    outline: OutlineCapture,
}

impl Ds {
    fn new(stage: Stage, paths: usize) -> Ds {
        let stage_name = stage.name.clone();
        Ds {
            state: State::scratch(),
            image: Image::default(),
            platform: None,
            stage,
            base: None,
            dispatched: false,
            resolved: false,
            on_build_init: false,
            deps: Vec::new(),
            build_args: Vec::new(),
            steps: Vec::new(),
            ctx_paths: BTreeSet::new(),
            paths,
            unregistered: false,
            stage_name,
            cmd_index: 0,
            cmd_is_on_build: false,
            cmd_total: 0,
            workdir_set: false,
            epoch: None,
            named: None,
            domains: Vec::new(),
            ignore_cache: false,
            outline: OutlineCapture::default(),
            entrypoint: Tracker::default(),
            cmd: Tracker::default(),
            healthcheck: Tracker::default(),
            agentfile: Vec::new(),
        }
    }
}

/// An `ARG`'s value and where it was declared, for the checks and the outline
/// (`argInfo`): its doc comment, its value, the arguments its default named, and its
/// place in its instruction, which settles the outline's order within a line.
#[derive(Debug, Clone)]
struct ArgInfo {
    key: Vec<u8>,
    doc: Vec<u8>,
    value: Option<Vec<u8>>,
    deps: BTreeSet<Vec<u8>>,
    location: Location,
    seq: usize,
}

/// A secret or SSH agent a stage's steps mount, by its ID: whether one is required, the
/// step that first does, and its place among that step's mounts.
type Mounted = BTreeMap<Vec<u8>, (bool, Location, usize)>;

/// What a stage's outline holds (`outlineCapture`): every argument it can see, those it
/// uses, and the secrets and SSH agents its steps mount, each where it was first named
/// (with its place among its step's mounts, for a line that names more than one).
#[derive(Debug, Clone, Default)]
struct OutlineCapture {
    all_args: BTreeMap<Vec<u8>, ArgInfo>,
    used: BTreeSet<Vec<u8>>,
    secrets: Mounted,
    ssh: Mounted,
}

struct Planner<'a> {
    opts: &'a Options,
    /// Whether progress names carry the platform (dockerui's MultiPlatformRequested).
    multi_platform: bool,
    /// What cache mounts' IDs are under (dockerui's CacheIDNamespace).
    cache_ns: Vec<u8>,
    /// What RUN steps are named (dockerui's Hostname).
    hostname: Vec<u8>,
    resolver: &'a dyn Resolver,
    lint: &'a Linter,
    shlex: Lex,
    target_platform: Platform,
    build_platforms: Vec<Platform>,
    global_args: EnvList,
    all_args: BTreeMap<Vec<u8>, ArgInfo>,
    /// The build args, SOURCE_DATE_EPOCH's as resolved (`setBuildArgValue`).
    build_args: BTreeMap<Vec<u8>, Vec<u8>>,
    /// SOURCE_DATE_EPOCH: seconds and nanoseconds.
    epoch: Option<(i64, u32)>,
    states: Vec<Ds>,
    by_name: BTreeMap<Vec<u8>, usize>,
    path_sets: Vec<BTreeSet<Vec<u8>>>,
    graph: Graph,
    /// The build context: a source the dispatch reads, its attributes set once every
    /// stage has said which of its paths it uses.
    context: Output,
    proxy: Option<llb::ProxyEnv>,
    /// The .dockerignore's patterns, once dispatch begins, if it has any.
    ignore: Option<crate::glob::PatternMatcher>,
    /// Local named contexts' sources, their attributes set once the stage that reads
    /// each has said which of its paths it uses: the source, the stage, its key and its
    /// local name.
    named_locals: Vec<(Output, usize, Vec<u8>, Vec<u8>)>,
    /// The vertices each stage's dispatch made: its index, and the range.
    made: Vec<(usize, std::ops::Range<usize>)>,
    /// Each agent's and harness's content, its files at the root, by name (Q19.1).
    contents: BTreeMap<Vec<u8>, State>,
    /// Each agent's and harness's OSI config, by name, where it came as an artifact: what
    /// `SKILL --from=<agent>` may take is what it lists.
    domain_configs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Each agent's and harness's source, as its stage expanded it.
    domain_sources: BTreeMap<Vec<u8>, (Vec<u8>, Vec<u8>)>,
}

/// The frontends shards' own is: docker/dockerfile at any tag, labs ones included, which at
/// 1.27.1 are built as mainline is (frontend/dockerfile/release/*/tags, both empty), and
/// the same frontend's upstream builds.
const FRONTENDS: [&str; 2] = ["docker/dockerfile", "docker/dockerfile-upstream"];

/// What builder.Build does with the frontend BUILDKIT_SYNTAX or `# syntax=` names, which is
/// to hand the build to it: the Dockerfile frontend's own is this one, and any other, which
/// shards cannot run, fails the build where it is named.
fn check_frontend(text: &[u8], build_args: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<(), Fail> {
    let ours = |r: &[u8]| {
        std::str::from_utf8(r)
            .ok()
            .and_then(|r| Reference::parse_normalized(r).ok())
            .is_some_and(|r| r.domain == "docker.io" && FRONTENDS.contains(&r.path.as_str()))
    };
    let refused = |r: &[u8]| {
        errb(&[
            b"shards cannot run frontend ",
            r,
            b": it builds Dockerfiles with its own port of docker/dockerfile 1.27.1",
        ])
    };
    if let Some(cmdline) = build_args.get(b"BUILDKIT_SYNTAX".as_slice()) {
        let r = parser::first_word(go::trim_space(cmdline));
        return match ours(&r) {
            true => Ok(()),
            false => Err(Fail::new(errb(&[
                b"failed with build-arg:BUILDKIT_SYNTAX = ",
                cmdline,
                b": ",
                &refused(&r),
            ]))),
        };
    }
    match parser::detect_syntax(text) {
        Some((r, _, line)) if !ours(&r) => Err(Fail::new(refused(&r)).at(&vec![(line, line)])),
        _ => Ok(()),
    }
}

/// What dockerui's Client.init takes from build args, each in place of the option it
/// names, the check's in place of `# check=` (dockerfile/1.27.1 frontend/dockerui).
struct Dockerui {
    multi_platform: bool,
    cache_ns: Vec<u8>,
    hostname: Vec<u8>,
    check: Option<lint::Config>,
}

/// Client.init's reading of BUILDKIT_MULTI_PLATFORM, BUILDKIT_CACHE_MOUNT_NS,
/// BUILDKIT_SANDBOX_HOSTNAME and BUILDKIT_DOCKERFILE_CHECK, in that order, its errors
/// dockerui's.
fn dockerui(opts: &Options) -> Result<Dockerui, Vec<u8>> {
    let arg = |k: &str| opts.build_args.get(k.as_bytes());
    let mut multi_platform = opts.multi_platform;
    if let Some(v) = arg("BUILDKIT_MULTI_PLATFORM").filter(|v| !v.is_empty()) {
        let b = go::parse_bool(v).ok_or_else(|| errb(&[b"invalid boolean value for multi-platform: ", v]))?;
        if !b && multi_platform {
            return Err(b"conflicting config: returning multiple target platforms is not allowed".to_vec());
        }
        multi_platform = b;
    }
    let hostname = match arg("BUILDKIT_SANDBOX_HOSTNAME") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => opts.hostname.clone(),
    };
    let check = arg("BUILDKIT_DOCKERFILE_CHECK")
        .map(|v| {
            lint::parse_options(v)
                .map_err(|e| errb(&[b"failed to parse build-arg:BUILDKIT_DOCKERFILE_CHECK: ", &e]))
        })
        .transpose()?;
    Ok(Dockerui {
        multi_platform,
        cache_ns: arg("BUILDKIT_CACHE_MOUNT_NS").cloned().unwrap_or_default(),
        hostname,
        check,
    })
}

/// Plans `text` as Dockerfile2LLB does.
pub fn plan(text: &[u8], opts: &Options, resolver: &dyn Resolver) -> Result<Plan, Error> {
    let fail = |message| Error {
        message,
        location: Vec::new(),
        warnings: Vec::new(),
    };
    let mut ui = dockerui(opts).map_err(fail)?;
    check_frontend(text, &opts.build_args).map_err(|Fail(message, location)| Error {
        message,
        location,
        warnings: Vec::new(),
    })?;
    let config = match ui.check.take() {
        Some(config) => config,
        None => {
            let check = parser::directive_value(text, b"check").unwrap_or_default();
            lint::parse_options(&check).map_err(|e| fail(errb(&[b"failed to parse check options: ", &e])))?
        }
    };
    let linter = Linter::new(config);
    let done = |r: Result<Plan, Fail>, linter: &Linter| match r {
        Ok(mut p) => {
            p.warnings = linter.warnings();
            Ok(p)
        }
        Err(Fail(message, location)) => Err(Error {
            message,
            location,
            warnings: linter.warnings(),
        }),
    };
    if text.is_empty() {
        return done(
            Err(Fail::new(b"the Dockerfile cannot be empty".to_vec())),
            &linter,
        );
    }
    let r = plan_with(text, opts, &ui, resolver, &linter, Mode::Plan).and_then(|p| match p {
        Planned::Plan(p) => Ok(*p),
        Planned::Outline(_) | Planned::Dispatched => Err(Fail::new(b"no plan".to_vec())),
    });
    done(r, &linter)
}

/// The outline of the build's target, as BuildKit's frontend answers `--call=outline`
/// (Dockerfile2Outline): planned as a build is, up to its steps, then its arguments,
/// secrets and SSH agents, and those of the stages it stands on and reads.
pub fn outline(text: &[u8], opts: &Options, resolver: &dyn Resolver) -> Result<Outline, Error> {
    let fail = |message| Error {
        message,
        location: Vec::new(),
        warnings: Vec::new(),
    };
    let mut ui = dockerui(opts).map_err(fail)?;
    check_frontend(text, &opts.build_args).map_err(|Fail(message, location)| Error {
        message,
        location,
        warnings: Vec::new(),
    })?;
    // The checks, as the plan's: a violation under `error=true` fails it.
    let config = match ui.check.take() {
        Some(config) => config,
        None => {
            let check = parser::directive_value(text, b"check").unwrap_or_default();
            lint::parse_options(&check).map_err(|e| fail(errb(&[b"failed to parse check options: ", &e])))?
        }
    };
    let linter = Linter::new(config);
    if text.is_empty() {
        return Err(fail(b"the Dockerfile cannot be empty".to_vec()));
    }
    match plan_with(text, opts, &ui, resolver, &linter, Mode::Outline) {
        Ok(Planned::Outline(o)) => Ok(o),
        Ok(Planned::Plan(_) | Planned::Dispatched) => Err(fail(b"no outline".to_vec())),
        Err(Fail(message, location)) => Err(Error {
            message,
            location,
            warnings: Vec::new(),
        }),
    }
}

/// The build's checks, as BuildKit's frontend answers `--call=check` (DockerfileLint):
/// every stage planned up to its steps, those the target does not reach too when no
/// target is named, and their warnings in the order they were found, with the error that
/// ended the planning, if one did, where its locations together say (mergeLocations).
pub fn lint(text: &[u8], opts: &Options, resolver: &dyn Resolver) -> Result<Lint, Error> {
    let mut opts = opts.clone();
    opts.all_stages = opts.target.is_empty();
    let fail = |message| Error {
        message,
        location: Vec::new(),
        warnings: Vec::new(),
    };
    // dockerui's settings and the frontend the file names fail the request itself, before
    // the subrequest is asked (builder.Build); what follows fails only the plan, which
    // the result says.
    let mut ui = dockerui(&opts).map_err(fail)?;
    check_frontend(text, &opts.build_args).map_err(|Fail(message, location)| Error {
        message,
        location,
        warnings: Vec::new(),
    })?;
    let failed = |message: Vec<u8>, location: &[Location], warnings| Lint {
        warnings,
        error: Some((message, merge_locations(location))),
    };
    if text.is_empty() {
        return Ok(failed(
            b"the Dockerfile cannot be empty".to_vec(),
            &[],
            Vec::new(),
        ));
    }
    let config = match ui.check.take() {
        Some(config) => config,
        None => {
            let check = parser::directive_value(text, b"check").unwrap_or_default();
            match lint::parse_options(&check) {
                Ok(config) => config,
                Err(e) => {
                    let message = errb(&[b"failed to parse check options: ", &e]);
                    return Ok(failed(message, &[], Vec::new()));
                }
            }
        }
    };
    let linter = Linter::new(config);
    Ok(match plan_with(text, &opts, &ui, resolver, &linter, Mode::Lint) {
        Ok(_) => Lint {
            warnings: linter.warnings(),
            error: None,
        },
        Err(Fail(message, location)) => failed(message, &location, linter.warnings()),
    })
}

/// What [`lint`] found: the warnings, and the error that ended the planning, with its
/// merged location.
#[derive(Debug, Clone)]
pub struct Lint {
    pub warnings: Vec<lint::Warning>,
    pub error: Option<(Vec<u8>, instructions::Location)>,
}

/// `mergeLocations`: every range of `locations`, by first line, those that overlap one
/// another made one.
fn merge_locations(locations: &[instructions::Location]) -> instructions::Location {
    let mut all: instructions::Location = locations.iter().flatten().copied().collect();
    // slices.SortFunc by first line; for the few ranges an error has, its insertion sort,
    // which keeps ranges of one first line in their order.
    all.sort_by_key(|r| r.0);
    let mut merged = instructions::Location::new();
    let mut ranges = all.into_iter();
    let Some(mut current) = ranges.next() else {
        return merged;
    };
    for r in ranges {
        if r.0 <= current.1 {
            current.1 = current.1.max(r.1);
        } else {
            merged.push(current);
            current = r;
        }
    }
    merged.push(current);
    merged
}

/// The file's stages, as BuildKit's frontend answers `--call=targets` (ListTargets):
/// each as written, its doc comment, the last the default. Unlike BuildKit's, a stage's
/// `check=` comment does not take the frontend down (ListTargets parses with no linter,
/// which that comment dereferences).
pub fn targets(text: &[u8], dialect: parser::Dialect) -> Result<Targets, Error> {
    let err = |message: Vec<u8>, location: Vec<Location>| Error {
        message,
        location,
        warnings: Vec::new(),
    };
    let parsed = parser::parse_as(text, dialect).map_err(|e| err(e.message, e.location))?;
    let linter = Linter::new(lint::Config::default());
    let ins = instructions::parse(&parsed, &linter).map_err(|e| err(e.message, e.location))?;
    let last = ins.stages.len().saturating_sub(1);
    Ok(Targets {
        targets: ins
            .stages
            .iter()
            .enumerate()
            .map(|(i, st)| Target {
                name: st.name.clone(),
                default: i == last,
                description: st.doc_comment.clone(),
                base: st.base_name.clone(),
                platform: st.platform.clone(),
                location: st.location.clone(),
            })
            .collect(),
        sources: vec![text.to_vec()],
    })
}

/// How far [`plan_with`] plans: the whole plan, the target's outline, or every stage
/// dispatched and no more, as the lint subrequest (DockerfileLint).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plan,
    Outline,
    Lint,
}

/// What [`plan_with`] made: the plan, the target's outline, or its stages dispatched.
enum Planned {
    Plan(Box<Plan>),
    Outline(Outline),
    Dispatched,
}

fn plan_with(
    text: &[u8],
    opts: &Options,
    ui: &Dockerui,
    resolver: &dyn Resolver,
    linter: &Linter,
    mode: Mode,
) -> Result<Planned, Fail> {
    if opts.target_platform.os != b"linux" {
        return Err(Fail::new(errb(&[
            b"shards builds Linux guests: the target platform ",
            &platform::format_all(&opts.target_platform),
            b" is not one",
        ])));
    }
    let parsed = parser::parse_as(text, opts.dialect).map_err(|e| Fail(e.message, e.location))?;
    for w in &parsed.warnings {
        if w.url == lint::NO_EMPTY_CONTINUATION.url {
            linter.run(
                &lint::NO_EMPTY_CONTINUATION,
                &[(w.line, w.line)],
                Some(b"Empty continuation line"),
            );
        }
    }
    let ins = instructions::parse(&parsed, linter).map_err(|e| Fail(e.message, e.location))?;
    if opts.dialect == parser::Dialect::Agentfile {
        crate::agentfile::check(&ins).map_err(|e| Fail(e.message, e.location))?;
    }
    if ins.stages.is_empty() {
        return Err(Fail::new(b"dockerfile contains no stages to build".to_vec()));
    }
    validate_stage_names(&ins.stages, linter);
    validate_command_casing(&ins.stages, linter);

    let build_platforms = if opts.build_platforms.is_empty() {
        vec![opts.target_platform.clone()]
    } else {
        opts.build_platforms.clone()
    };
    let target_name = if opts.target.is_empty() {
        ins.stages.last().map(|s| s.name.clone()).unwrap_or_default()
    } else {
        opts.target.clone()
    };
    let defaults = default_args(
        &build_platforms,
        &opts.target_platform,
        &opts.build_args,
        &target_name,
    );
    let shlex = Lex::new(u32::from(parsed.escape));
    let arg_cmds: Vec<&Command> = ins.meta_args.iter().collect();

    validate_base_images_with_default_args(&ins.stages, &shlex, &defaults, &arg_cmds, linter);
    let (global_args, all_args) = build_meta_args(defaults, &shlex, &arg_cmds, Some(&opts.build_args))?;

    let mut epoch = None;
    let mut global_args = global_args;
    let mut build_args = opts.build_args.clone();
    if let Some(v) = build_arg_value(&build_args, &global_args, b"SOURCE_DATE_EPOCH") {
        epoch = resolve_epoch(&v, &ins.stages, &build_args, &global_args, &shlex, resolver)?;
        let formatted = epoch.map(|(s, _)| s.to_string().into_bytes()).unwrap_or_default();
        set_build_arg_value(
            &mut build_args,
            &mut global_args,
            b"SOURCE_DATE_EPOCH",
            &formatted,
        );
    }

    let mut graph = Graph::default();
    let context = graph
        .source(
            b"local://context".to_vec(),
            BTreeMap::new(),
            None,
            Meta::default(),
        )
        .output
        .ok_or_else(|| Fail::new(b"no build context".to_vec()))?;
    let mut p = Planner {
        opts,
        resolver,
        lint: linter,
        multi_platform: ui.multi_platform,
        cache_ns: ui.cache_ns.clone(),
        hostname: ui.hostname.clone(),
        shlex,
        target_platform: opts.target_platform.clone(),
        build_platforms,
        global_args,
        all_args,
        proxy: proxy_env(&build_args),
        build_args,
        epoch,
        states: Vec::new(),
        by_name: BTreeMap::new(),
        path_sets: Vec::new(),
        graph,
        context,
        ignore: None,
        named_locals: Vec::new(),
        made: Vec::new(),
        contents: BTreeMap::new(),
        domain_configs: BTreeMap::new(),
        domain_sources: BTreeMap::new(),
    };
    p.build_dispatch_states(ins.stages)?;
    let target = p.resolve_target()?;
    p.build_stage_dependency_graph()?;
    let reachable = p.resolve_stages(target)?;
    p.dispatch_stages(&reachable, target)?;
    if mode != Mode::Plan {
        if p.lint.failed() {
            return Err(Fail::new(p.lint.error_message()));
        }
        if mode == Mode::Lint {
            return Ok(Planned::Dispatched);
        }
        return Ok(Planned::Outline(p.outline_of(target, text)));
    }
    p.finalize(target).map(|plan| Planned::Plan(Box::new(plan)))
}

/// `getBuildArgValue`.
fn build_arg_value(build_args: &BTreeMap<Vec<u8>, Vec<u8>>, global: &EnvList, key: &[u8]) -> Option<Vec<u8>> {
    if let Some(v) = build_args.get(key) {
        return Some(v.clone());
    }
    global.get(key).filter(|v| !v.is_empty()).map(<[u8]>::to_vec)
}

/// `setBuildArgValue`: `value` for `key` in the build args and the global args, in each
/// only if it holds the key; nothing removes it.
fn set_build_arg_value(
    build_args: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    global: &mut EnvList,
    key: &[u8],
    value: &[u8],
) {
    if build_args.contains_key(key) {
        if value.is_empty() {
            build_args.remove(key);
        } else {
            build_args.insert(key.to_vec(), value.to_vec());
        }
    }
    if global.get(key).is_some() {
        if value.is_empty() {
            global.delete(key);
        } else {
            global.add(key, value);
        }
    }
}

/// `resolveSourceDateEpochValue`: a number of seconds (`strconv.ParseInt`, which reads a
/// sign and digits as i64's parse does), or a source to take the time from.
fn resolve_epoch(
    v: &[u8],
    stages: &[Stage],
    build_args: &BTreeMap<Vec<u8>, Vec<u8>>,
    global_args: &EnvList,
    shlex: &Lex,
    resolver: &dyn Resolver,
) -> Result<Option<(i64, u32)>, Fail> {
    if v.is_empty() {
        return Ok(None);
    }
    if let Some(n) = std::str::from_utf8(v).ok().and_then(|s| s.parse::<i64>().ok()) {
        return Ok(Some((n, 0)));
    }
    let source = epoch_source(v, stages, build_args, global_args, shlex)?;
    resolver.epoch(&source).map_err(Fail::new)
}

/// `llb.Git`'s identifier (`git://HOST/PATH[#REF[:SUBDIR]]`) and attributes for `g`, as
/// BuildKit makes them. Unlike BuildKit, no SSH host keys are scanned while planning: they
/// are the fetch's to verify.
pub fn git_identifier(
    g: &git::GitRef,
    keep_git_dir: bool,
    checksum: &[u8],
) -> (Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>) {
    let mut full = g.remote.clone();
    let mut remote = git::parse_url(&full);
    if remote == Err(git::UrlError::UnknownProtocol) {
        full = [b"https://".as_slice(), &full].concat();
        remote = git::parse_url(&full);
    }
    if let Ok(r) = &remote {
        full = r.remote.clone();
    }
    let id = match &remote {
        Err(_) => full.clone(),
        Ok(r) => {
            let mut id = [r.host.as_slice(), &go::join(&[b"/", &r.path])].concat();
            if !g.reference.is_empty() || !g.subdir.is_empty() {
                id.push(b'#');
                id.extend_from_slice(&g.reference);
                if !g.subdir.is_empty() {
                    id.push(b':');
                    id.extend_from_slice(&g.subdir);
                }
            }
            id
        }
    };
    let mut attrs = BTreeMap::new();
    if keep_git_dir {
        attrs.insert(b"git.keepgitdir".to_vec(), b"true".to_vec());
    }
    if !full.is_empty() {
        attrs.insert(b"git.fullurl".to_vec(), full);
    }
    attrs.insert(b"git.authtokensecret".to_vec(), b"GIT_AUTH_TOKEN".to_vec());
    attrs.insert(b"git.authheadersecret".to_vec(), b"GIT_AUTH_HEADER".to_vec());
    if remote.as_ref().is_ok_and(|r| r.scheme == b"ssh") {
        attrs.insert(b"git.mountsshsock".to_vec(), b"default".to_vec());
    }
    if !checksum.is_empty() {
        attrs.insert(b"git.checksum".to_vec(), checksum.to_vec());
    }
    if g.submodules == Some(false) {
        attrs.insert(b"git.skipsubmodules".to_vec(), b"true".to_vec());
    }
    if g.fetch_by_commit {
        attrs.insert(b"git.fetchbycommit".to_vec(), b"true".to_vec());
    }
    ([b"git://".as_slice(), &id].concat(), attrs)
}

/// `resolveSourceDateEpochState`. shards takes no named contexts (`--build-context`), so
/// the name is the context's or a stage's.
fn epoch_source(
    v: &[u8],
    stages: &[Stage],
    build_args: &BTreeMap<Vec<u8>, Vec<u8>>,
    global_args: &EnvList,
    shlex: &Lex,
) -> Result<EpochSource, Fail> {
    if v == b"context" {
        return Ok(EpochSource::Context);
    }
    let Some(stage) = stages.iter().find(|s| equal_fold_name(&s.name, v)) else {
        return Err(Fail::new(errb(&[b"invalid SOURCE_DATE_EPOCH: ", v])));
    };
    let mut args = global_args.clone();
    args.delete(b"SOURCE_DATE_EPOCH");
    epoch_stage_source(stage, build_args, args, shlex).map_err(|f| f.at(&stage.location))
}

/// `strings.EqualFold(name, v)` for a stage's name, which is lowercase ASCII: each of
/// `v`'s runes is the name's byte in either case, or the one other rune that folds to it,
/// the Kelvin sign to k and the long s to s (Unicode's CaseFolding.txt).
fn equal_fold_name(name: &[u8], v: &[u8]) -> bool {
    let mut runes = go::runes(v);
    for &b in name {
        let Some((r, _)) = runes.next() else {
            return false;
        };
        let folds = match b {
            b'k' => r == 0x212A,
            b's' => r == 0x17F,
            _ => false,
        };
        if !(r == u32::from(b) || r == u32::from(b.to_ascii_uppercase()) || folds) {
            return false;
        }
    }
    runes.next().is_none()
}

/// `sourceDateEpochStageSource`: a stage `FROM scratch` that only fetches the source, its
/// ARGs set as they come and one remote ADD.
fn epoch_stage_source(
    stage: &Stage,
    build_args: &BTreeMap<Vec<u8>, Vec<u8>>,
    global_args: EnvList,
    shlex: &Lex,
) -> Result<EpochSource, Fail> {
    let one = || Fail::new(b"SOURCE_DATE_EPOCH stage must contain exactly one remote ADD".to_vec());
    let base = shlex.process(&stage.base_name, &global_args).map_err(|e| {
        Fail::new(errb(&[
            b"failed to process source stage base name ",
            go::quote(&stage.base_name).as_bytes(),
            b": ",
            &e.0,
        ]))
    })?;
    if base.word != b"scratch" {
        return Err(Fail::new(
            b"SOURCE_DATE_EPOCH stage must use FROM scratch".to_vec(),
        ));
    }
    let mut env = global_args;
    let mut source = None;
    for cmd in &stage.commands {
        match &cmd.kind {
            Kind::Arg(defs) => {
                for arg in defs {
                    if let Some(v) = build_args.get(&arg.key) {
                        env.add(&arg.key, v);
                    } else if let Some(value) = &arg.value {
                        let v = shlex.process(value, &env)?.word;
                        env.add(&arg.key, &v);
                    }
                }
            }
            Kind::Add(add) => {
                if source.is_some() {
                    return Err(one());
                }
                source = Some(epoch_add_source(&stage.name, add, &env, shlex)?);
            }
            _ => {
                return Err(Fail::new(errb(&[
                    b"SOURCE_DATE_EPOCH stage does not meet source-only requirements: unsupported ",
                    &cmd.name,
                    b" instruction",
                ])));
            }
        }
    }
    source.ok_or_else(one)
}

/// `sourceDateEpochAddSource`.
fn epoch_add_source(
    stage: &[u8],
    add: &instructions::Add,
    env: &EnvList,
    shlex: &Lex,
) -> Result<EpochSource, Fail> {
    let ([path], []) = (add.sources.paths.as_slice(), add.sources.contents.as_slice()) else {
        return Err(Fail::new(
            b"SOURCE_DATE_EPOCH stage must contain exactly one remote ADD source".to_vec(),
        ));
    };
    let src = shlex.process(path, env)?.word;
    if is_http_source(&src) {
        let checksum = if add.checksum.is_empty() {
            None
        } else {
            Some(parse_checksum(&shlex.process(&add.checksum, env)?.word)?)
        };
        return Ok(EpochSource::Http {
            stage: stage.to_vec(),
            filename: http_filename(&src),
            url: src,
            checksum,
        });
    }
    match git::parse_git_ref(&src) {
        git::Parsed::BadGit(e) => Err(Fail::new(e)),
        git::Parsed::Git(mut git) if !git.indistinguishable_from_local => {
            if !add.checksum.is_empty() {
                git.checksum = shlex.process(&add.checksum, env)?.word;
            }
            Ok(EpochSource::Git {
                stage: stage.to_vec(),
                git,
            })
        }
        _ => Err(Fail::new(
            b"SOURCE_DATE_EPOCH stage source must be a single HTTP(S) or Git ADD".to_vec(),
        )),
    }
}

/// The name a URL's file takes: the base of its path, or `__unnamed__`.
fn http_filename(src: &[u8]) -> Vec<u8> {
    match url::parse(src) {
        Ok(u) => {
            let base = path_base(&u.path);
            if base != b"." && base != b"/" {
                base
            } else {
                b"__unnamed__".to_vec()
            }
        }
        Err(_) => b"__unnamed__".to_vec(),
    }
}

/// `digest.Parse`, its errors go-digest's.
fn parse_checksum(c: &[u8]) -> Result<Vec<u8>, Fail> {
    let c = std::str::from_utf8(c).map_err(|_| Fail::new(b"invalid checksum digest format".to_vec()))?;
    let dg = shards_image::reference::Digest::parse(c).map_err(|e| Fail::new(e.to_string().into_bytes()))?;
    Ok(dg.to_string().into_bytes())
}

/// `defaultArgs`: the platform arguments every build has.
fn default_args(
    build: &[Platform],
    target: &Platform,
    overrides: &BTreeMap<Vec<u8>, Vec<u8>>,
    target_stage: &[u8],
) -> EnvList {
    let bp = build.first().cloned().unwrap_or_default();
    let stage: &[u8] = if target_stage.is_empty() {
        b"default"
    } else {
        target_stage
    };
    let pairs: [(&[u8], Vec<u8>); 11] = [
        (b"BUILDPLATFORM", platform::format(&bp)),
        (b"BUILDOS", bp.os.clone()),
        (b"BUILDOSVERSION", bp.os_version.clone()),
        (b"BUILDARCH", bp.architecture.clone()),
        (b"BUILDVARIANT", bp.variant.clone()),
        (b"TARGETPLATFORM", platform::format_all(target)),
        (b"TARGETOS", target.os.clone()),
        (b"TARGETOSVERSION", target.os_version.clone()),
        (b"TARGETARCH", target.architecture.clone()),
        (b"TARGETVARIANT", target.variant.clone()),
        (b"TARGETSTAGE", stage.to_vec()),
    ];
    let mut env = EnvList::default();
    for (k, v) in pairs {
        let v = overrides.get(k).cloned().unwrap_or(v);
        env.add(k, &v);
    }
    env
}

/// `buildMetaArgs`: the `ARG`s before the first stage, with the build's values. Without
/// build arguments, as if none were given (for the checks of defaults).
fn build_meta_args(
    mut args: EnvList,
    shlex: &Lex,
    cmds: &[&Command],
    build_args: Option<&BTreeMap<Vec<u8>, Vec<u8>>>,
) -> Result<(EnvList, BTreeMap<Vec<u8>, ArgInfo>), Fail> {
    let mut all = BTreeMap::new();
    for cmd in cmds {
        let Kind::Arg(defs) = &cmd.kind else {
            continue;
        };
        for (seq, kp) in defs.iter().enumerate() {
            let mut deps = BTreeSet::new();
            let value = match build_args.and_then(|b| b.get(&kp.key)) {
                Some(v) => Some(v.clone()),
                None => match &kp.value {
                    Some(v) => {
                        let processed = shlex
                            .process(v, &args)
                            .map_err(|e| Fail::from(e).at(&cmd.location))?;
                        deps = processed.matched;
                        // A default naming itself takes on what its earlier one named.
                        if deps.remove(&kp.key)
                            && let Some(old) = all.get(&kp.key)
                        {
                            let old: &ArgInfo = old;
                            deps.extend(old.deps.iter().cloned());
                        }
                        Some(processed.word)
                    }
                    None => None,
                },
            };
            if let Some(v) = &value {
                args.add(&kp.key, v);
            }
            all.insert(
                kp.key.clone(),
                ArgInfo {
                    key: kp.key.clone(),
                    doc: kp.doc_comment.clone(),
                    value,
                    deps,
                    location: cmd.location.clone(),
                    seq,
                },
            );
        }
    }
    Ok((args, all))
}

/// `proxyEnvFromBuildArgs`: the proxy variables among the build arguments, any case.
fn proxy_env(args: &BTreeMap<Vec<u8>, Vec<u8>>) -> Option<llb::ProxyEnv> {
    let mut pe = llb::ProxyEnv::default();
    let mut any = false;
    for (k, v) in args {
        let slot = match go::to_lower(k).as_slice() {
            b"http_proxy" => &mut pe.http,
            b"https_proxy" => &mut pe.https,
            b"ftp_proxy" => &mut pe.ftp,
            b"no_proxy" => &mut pe.no,
            b"all_proxy" => &mut pe.all,
            _ => continue,
        };
        *slot = v.clone();
        any = true;
    }
    any.then_some(pe)
}

fn validate_stage_names(stages: &[Stage], lint: &Linter) {
    let mut seen = BTreeSet::new();
    for s in stages {
        if s.name.is_empty() {
            continue;
        }
        if s.name == b"context" || s.name == b"scratch" {
            let msg = errb(&[
                b"Stage name should not use the same name as reserved stage ",
                go::quote(&s.name).as_bytes(),
            ]);
            lint.run(&lint::RESERVED_STAGE_NAME, &s.location, Some(&msg));
        }
        if !seen.insert(s.name.clone()) {
            let msg = errb(&[
                b"Duplicate stage name ",
                go::quote(&s.name).as_bytes(),
                b", stage names should be unique",
            ]);
            lint.run(&lint::DUPLICATE_STAGE_NAME, &s.location, Some(&msg));
        }
    }
}

fn validate_command_casing(stages: &[Stage], lint: &Linter) {
    let consistent = |s: &[u8]| s == go::to_lower(s).as_slice() || s == go::to_upper(s).as_slice();
    let (mut lower, mut upper) = (0, 0);
    let mut count = |name: &[u8]| {
        if consistent(name) {
            if name == go::to_lower(name).as_slice() {
                lower += 1;
            } else {
                upper += 1;
            }
        }
    };
    for s in stages {
        count(&s.orig_cmd);
        for c in &s.commands {
            count(&c.name);
        }
    }
    let majority_lower = lower > upper;
    let check = |name: &[u8], loc: &Location| {
        let casing: &[u8] = if majority_lower && go::to_lower(name) != name {
            b"lowercase"
        } else if !majority_lower && go::to_upper(name) != name {
            b"uppercase"
        } else {
            return;
        };
        let msg = errb(&[
            b"Command '",
            name,
            b"' should match the case of the command majority (",
            casing,
            b")",
        ]);
        lint.run(&lint::CONSISTENT_INSTRUCTION_CASING, loc, Some(&msg));
    };
    for s in stages {
        check(&s.orig_cmd, &s.location);
        for c in &s.commands {
            check(&c.name, &c.location);
        }
    }
}

fn validate_base_images_with_default_args(
    stages: &[Stage],
    shlex: &Lex,
    env: &EnvList,
    arg_cmds: &[&Command],
    lint: &Linter,
) {
    let Ok((args, _)) = build_meta_args(env.clone(), shlex, arg_cmds, None) else {
        return;
    };
    for st in stages {
        let Ok(name) = shlex.process(&st.base_name, &args) else {
            return;
        };
        if parse_normalized(&name.word).is_err() {
            let msg = errb(&[
                b"Default value for ARG ",
                &st.base_name,
                b" results in empty or invalid base image name",
            ]);
            lint.run(&lint::INVALID_DEFAULT_ARG_IN_FROM, &st.location, Some(&msg));
        }
    }
}

/// `reference.ParseNormalizedNamed`, for a name of any bytes.
fn parse_normalized(name: &[u8]) -> Result<Reference, Vec<u8>> {
    let s = std::str::from_utf8(name).map_err(|_| b"invalid reference format".to_vec())?;
    Reference::parse_normalized(s).map_err(|e| e.to_string().into_bytes())
}

/// `reportUnusedFromArgs`: each variable a `FROM` names that no `ARG` declared.
fn report_unused_from_args(
    known: &BTreeSet<Vec<u8>>,
    unmatched: &BTreeSet<Vec<u8>>,
    loc: &Location,
    lint: &LinterView<'_>,
) {
    let options: Vec<&[u8]> = known.iter().map(Vec::as_slice).collect();
    for arg in unmatched {
        if known.contains(arg) {
            continue;
        }
        let mut msg = errb(&[b"FROM argument '", arg, b"' is not declared"]);
        if let Some(s) = instructions::suggest(arg, &options, true) {
            msg.extend_from_slice(&errb(&[b" (did you mean ", &s, b"?)"]));
        }
        lint.run(&lint::UNDEFINED_ARG_IN_FROM, loc, Some(&msg));
    }
}

/// `validateNoSecretKey`: a key that reads as a secret's name.
fn validate_no_secret_key(instruction: &[u8], key: &[u8], loc: &Location, lint: &LinterView<'_>) {
    const DENY: &[&[u8]] = &[
        b"apikey",
        b"auth",
        b"credential",
        b"credentials",
        b"key",
        b"password",
        b"pword",
        b"passwd",
        b"secret",
        b"token",
    ];
    const ALLOW: &[&[u8]] = &[b"public", b"file", b"version"];
    // `(?i)(?:_|^)(?:word)(?:_|$)`: a word that is the whole key, or begins or ends it at
    // an underscore, or stands between two. Go's regexp folds case by Unicode's simple
    // case folding, the orbit of each of these ASCII letters being its two cases, and for
    // k and s a third: the Kelvin sign and the long s (CaseFolding.txt).
    let runes: Vec<u32> = go::runes(key).map(|(r, _)| r).collect();
    let folds = |r: u32, c: u8| {
        r == u32::from(c)
            || r == u32::from(c.to_ascii_uppercase())
            || (c == b'k' && r == 0x212A)
            || (c == b's' && r == 0x17F)
    };
    let underscore = |i: usize| runes.get(i) == Some(&u32::from(b'_'));
    let has = |words: &[&[u8]]| {
        (0..runes.len()).any(|i| {
            (i == 0 || underscore(i - 1))
                && words.iter().any(|w| {
                    let end = i + w.len();
                    runes
                        .get(i..end)
                        .is_some_and(|run| run.iter().zip(w.iter()).all(|(&r, &c)| folds(r, c)))
                        && (end == runes.len() || underscore(end))
                })
        })
    };
    if has(DENY) && !has(ALLOW) {
        let msg = errb(&[
            b"Do not use ARG or ENV instructions for sensitive data (",
            instruction,
            b" ",
            go::quote(key).as_bytes(),
            b")",
        ]);
        lint.run(&lint::SECRETS_USED_IN_ARG_OR_ENV, loc, Some(&msg));
    }
}

impl Planner<'_> {
    fn new_paths(&mut self) -> usize {
        self.path_sets.push(BTreeSet::new());
        self.path_sets.len() - 1
    }

    /// `dispatchStates.addState`: its base is the stage its base name names.
    /// `dispatchState.Outline`: the target's arguments, secrets and SSH agents, then its
    /// base's and the stages' it reads, each named once, ordered by where it was first
    /// named (within a line, by its place there, where BuildKit's order is a map's).
    fn outline_of(&self, target: usize, text: &[u8]) -> Outline {
        let mut args: Vec<&ArgInfo> = Vec::new();
        self.outline_args(target, &mut BTreeSet::new(), &mut args);
        args.sort_by_key(|a| (a.location.first().map_or(0, |r| r.0), a.seq));
        let mounts = |pick: fn(&OutlineCapture) -> &Mounted| {
            let mut found: Vec<(Vec<u8>, bool, Location, usize)> = Vec::new();
            self.outline_mounts(target, pick, &mut BTreeSet::new(), &mut found);
            found.sort_by_key(|(_, _, loc, seq)| (loc.first().map_or(0, |r| r.0), *seq));
            found
                .into_iter()
                .map(|(name, required, location, _)| subrequests::Mount {
                    name,
                    required,
                    location,
                })
                .collect()
        };
        let stage = self.states.get(target).map(|ds| &ds.stage);
        Outline {
            name: stage.map(|s| s.name.clone()).unwrap_or_default(),
            description: stage.map(|s| s.doc_comment.clone()).unwrap_or_default(),
            args: args
                .into_iter()
                .map(|a| Arg {
                    name: a.key.clone(),
                    description: a.doc.clone(),
                    value: a.value.clone().unwrap_or_default(),
                    location: a.location.clone(),
                })
                .collect(),
            secrets: mounts(|o| &o.secrets),
            ssh: mounts(|o| &o.ssh),
            sources: vec![text.to_vec()],
        }
    }

    /// `dispatchState.args`: the arguments stage `d` uses, those their defaults name
    /// (markAllUsed), then its base's and its dependencies', each once.
    fn outline_args<'s>(&'s self, d: usize, visited: &mut BTreeSet<Vec<u8>>, out: &mut Vec<&'s ArgInfo>) {
        let Some(ds) = self.states.get(d) else { return };
        let o = &ds.outline;
        let mut used = o.used.clone();
        let mut pending: Vec<Vec<u8>> = used.iter().cloned().collect();
        while let Some(k) = pending.pop() {
            if let Some(a) = o.all_args.get(&k) {
                for dep in &a.deps {
                    if used.insert(dep.clone()) {
                        pending.push(dep.clone());
                    }
                }
            }
        }
        for k in &used {
            if let Some(a) = o.all_args.get(k)
                && visited.insert(k.clone())
            {
                out.push(a);
            }
        }
        if let Some(b) = ds.base {
            self.outline_args(b, visited, out);
        }
        for (dep, _) in &ds.deps {
            self.outline_args(*dep, visited, out);
        }
    }

    /// `dispatchState.secrets` and `.ssh`: stage `d`'s, then its base's and its
    /// dependencies', each once.
    fn outline_mounts(
        &self,
        d: usize,
        pick: fn(&OutlineCapture) -> &Mounted,
        visited: &mut BTreeSet<Vec<u8>>,
        out: &mut Vec<(Vec<u8>, bool, Location, usize)>,
    ) {
        let Some(ds) = self.states.get(d) else { return };
        for (id, (required, loc, seq)) in pick(&ds.outline) {
            if visited.insert(id.clone()) {
                out.push((id.clone(), *required, loc.clone(), *seq));
            }
        }
        if let Some(b) = ds.base {
            self.outline_mounts(b, pick, visited, out);
        }
        for (dep, _) in &ds.deps {
            self.outline_mounts(*dep, pick, visited, out);
        }
    }

    fn add_state(&mut self, mut ds: Ds) -> usize {
        if let Some(&b) = self.by_name.get(&ds.stage.base_name) {
            ds.base = Some(b);
            if let Some(base) = self.states.get(b) {
                ds.outline = base.outline.clone();
            }
        }
        let name = go::to_lower(&ds.stage.name);
        let i = self.states.len();
        self.states.push(ds);
        if !name.is_empty() {
            self.by_name.insert(name, i);
        }
        i
    }

    fn names(&self) -> Vec<Vec<u8>> {
        self.states
            .iter()
            .filter(|s| !s.stage_name.is_empty())
            .map(|s| s.stage_name.clone())
            .collect()
    }

    fn build_dispatch_states(&mut self, stages: Vec<Stage>) -> Result<(), Fail> {
        let known: BTreeSet<Vec<u8>> = self
            .all_args
            .values()
            .map(|a| a.key.clone())
            .chain(self.global_args.keys().map(<[u8]>::to_vec))
            .collect();
        for (i, mut st) in stages.into_iter().enumerate() {
            let lint = self.lint.with_comments(&st.comments);
            let name = self.shlex.process(&st.base_name, &self.global_args);
            if let Ok(n) = &name {
                report_unused_from_args(&known, &n.unmatched, &st.location, &lint);
            }
            let name = name.map_err(|e| Fail::from(e).at(&st.location))?;
            // The arguments its FROM names, which its outline uses.
            let mut used = name.matched.clone();
            if name.word.is_empty() {
                return Err(
                    Fail::new(errb(&[b"base name (", &st.base_name, b") should not be blank"]))
                        .at(&st.location),
                );
            }
            st.base_name = name.word;
            let paths = self.new_paths();
            let mut ds = Ds::new(st, paths);
            ds.epoch = self.epoch;
            ds.outline.all_args = self.all_args.clone();
            if !ds.stage.platform.is_empty() {
                let v = ds.stage.platform.clone();
                let m = self.shlex.process(&v, &self.global_args);
                if let Ok(m) = &m {
                    report_unused_from_args(&known, &m.unmatched, &ds.stage.location, &lint);
                    self.report_redundant_target_platform(&v, m, &ds.stage.location, &lint);
                    report_const_platform_disallowed(
                        &ds.stage.name,
                        m,
                        &ds.stage.location,
                        &lint,
                        &self.target_platform,
                    );
                }
                let m = m.map_err(|e| {
                    Fail::new(errb(&[b"failed to process arguments for platform : ", &e.0]))
                        .at(&ds.stage.location)
                })?;
                if m.word.is_empty() {
                    let e = errb(&[b"empty platform value from expression ", &v]);
                    let e = self.wrap_suggest_any(e, &m.unmatched);
                    return Err(Fail::new(e).at(&ds.stage.location));
                }
                let build = self.build_platforms.first().cloned().unwrap_or_default();
                let p = platform::parse(&m.word, &build).map_err(|e| {
                    // Located once with the parse error, and again around it.
                    let e = self.wrap_suggest_any(e, &m.unmatched);
                    Fail::new(errb(&[b"failed to parse platform ", &v, b": ", &e]))
                        .at(&ds.stage.location)
                        .at(&ds.stage.location)
                })?;
                ds.platform = Some(p);
                used.extend(m.matched.iter().cloned());
            }
            if !ds.stage.name.is_empty() {
                let platform = ds.platform.clone();
                if let Some(n) = self.named_context(&ds.stage.name, platform.as_ref())? {
                    ds.named = Some(n);
                    let idx = self.add_state(ds);
                    if let Some(d) = self.states.get_mut(idx) {
                        d.base = None;
                    }
                    continue;
                }
            }
            if ds.stage.name.is_empty() {
                ds.stage_name = format!("stage-{i}").into_bytes();
            }
            let idx = self.add_state(ds);
            let ds = self
                .states
                .get_mut(idx)
                .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
            ds.outline.used.extend(used);
            let mut total = usize::from(ds.stage.base_name != EMPTY_IMAGE && ds.base.is_none());
            for c in &ds.stage.commands {
                use crate::agentfile::Directive;
                total += match &c.kind {
                    Kind::Add(_) | Kind::Copy(_) | Kind::Run(_) | Kind::Workdir(_) => 1,
                    Kind::Agentfile(Directive::Agent(a) | Directive::Harness(a)) => {
                        if matches!(
                            crate::agentfile::source_of(&a.source),
                            Ok(crate::agentfile::Source::Oci(_))
                        ) {
                            2
                        } else {
                            1
                        }
                    }
                    Kind::Agentfile(Directive::Mcp(m)) => match crate::agentfile::source_of(&m.source) {
                        Ok(crate::agentfile::Source::Http(_)) | Err(_) => 0,
                        Ok(crate::agentfile::Source::Oci(_)) => 2 * m.scope.names.len().max(1),
                        _ => m.scope.names.len().max(1),
                    },
                    // Its fetch, its check, and a layer where each goes.
                    Kind::Agentfile(Directive::Skill(sk)) => 2 + sk.scope.names.len().max(1),
                    _ => 0,
                };
            }
            ds.cmd_total = total;
            // dockerui's IsNoCache: by the stage's name, any case; every stage where the
            // option names none.
            ds.ignore_cache = self.opts.no_cache.as_ref().is_some_and(|names| {
                names.is_empty() || names.iter().any(|n| n.eq_ignore_ascii_case(&ds.stage.name))
            });
        }
        Ok(())
    }

    /// `wrapSuggestAny`: a suggestion for the first unmatched variable near an argument.
    fn wrap_suggest_any(&self, err: Vec<u8>, unmatched: &BTreeSet<Vec<u8>>) -> Vec<u8> {
        let keys: Vec<Vec<u8>> = self.global_args.keys().map(<[u8]>::to_vec).collect();
        let options: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        for k in unmatched {
            if instructions::suggest(k, &options, true).is_some() {
                return instructions::with_suggestion(err, k, &options, true);
            }
        }
        err
    }

    fn report_redundant_target_platform(
        &self,
        written: &[u8],
        m: &lex::Processed,
        loc: &Location,
        lint: &LinterView<'_>,
    ) {
        if m.matched.len() == 1
            && m.unmatched.is_empty()
            && m.matched.contains(b"TARGETPLATFORM".as_slice())
            && self.global_args.get(b"TARGETPLATFORM") == Some(m.word.as_slice())
        {
            let msg = errb(&[
                b"Setting platform to predefined ",
                written,
                b" in FROM is redundant as this is the default behavior",
            ]);
            lint.run(&lint::REDUNDANT_TARGET_PLATFORM, loc, Some(&msg));
        }
    }

    fn resolve_target(&self) -> Result<usize, Fail> {
        if self.opts.target.is_empty() {
            return self
                .states
                .len()
                .checked_sub(1)
                .ok_or_else(|| Fail::new(b"no stages".to_vec()));
        }
        match self.by_name.get(&go::to_lower(&self.opts.target)) {
            Some(&t) => Ok(t),
            None => {
                let e = errb(&[
                    b"target stage ",
                    go::quote(&self.opts.target).as_bytes(),
                    b" could not be found",
                ]);
                let names = self.names();
                let options: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
                Err(Fail::new(instructions::with_suggestion(
                    e,
                    &self.opts.target,
                    &options,
                    true,
                )))
            }
        }
    }

    /// `toCommand`: a step and the stages it reads.
    fn step_of(&mut self, command: Command) -> Result<Step, Fail> {
        let mut sources = Vec::new();
        match &command.kind {
            // An agent's or harness's content, which no stage is (Q19.1).
            Kind::Copy(c) if !c.from.is_empty() && self.declares_domain(&c.from) => {}
            Kind::Copy(c) if !c.from.is_empty() => {
                let from = c.from.clone();
                let expanded = self.shlex.process(&from, &lex::NoEnv)?;
                if expanded.word != from {
                    return Err(Fail::new(
                        b"variable expansion is not supported for --from, define a new stage with FROM using ARG from global scope as a workaround".to_vec(),
                    ));
                }
                let s = match std::str::from_utf8(&from)
                    .ok()
                    .and_then(|f| f.parse::<i64>().ok())
                {
                    Some(i) => {
                        if i < 0 || i as usize >= self.states.len() {
                            return Err(Fail::new(format!("invalid stage index {i}").into_bytes()));
                        }
                        i as usize
                    }
                    None => self.stage_or_unregistered(&from, &command.location),
                };
                sources.push(s);
            }
            Kind::Agentfile(crate::agentfile::Directive::Skill(sk)) if !sk.from.is_empty() => {
                let from = sk.from.clone();
                // An agent's or harness's skill: its content, which no stage is (Q19.6).
                if self.declares_domain(&from) {
                    return Ok(Step {
                        command,
                        sources,
                        on_build: false,
                    });
                }
                let s = match std::str::from_utf8(&from)
                    .ok()
                    .and_then(|f| f.parse::<i64>().ok())
                {
                    Some(i) if i >= 0 && (i as usize) < self.states.len() => i as usize,
                    Some(i) => return Err(Fail::new(format!("invalid stage index {i}").into_bytes())),
                    None => self.stage_or_unregistered(&from, &command.location),
                };
                sources.push(s);
            }
            Kind::Run(r) => {
                for m in &r.mounts {
                    let from: &[u8] = if m.from.is_empty() { EMPTY_IMAGE } else { &m.from };
                    let s = self.stage_or_unregistered(from, &Location::new());
                    sources.push(s);
                }
            }
            _ => {}
        }
        Ok(Step {
            command,
            sources,
            on_build: false,
        })
    }

    /// The stage named `name`, or a new unregistered one built on the image of that name.
    fn stage_or_unregistered(&mut self, name: &[u8], loc: &Location) -> usize {
        if let Some(&s) = self.by_name.get(&go::to_lower(name)) {
            return s;
        }
        let stage = Stage {
            name: Vec::new(),
            orig_cmd: Vec::new(),
            base_name: name.to_vec(),
            platform: Vec::new(),
            doc_comment: Vec::new(),
            source_code: Vec::new(),
            location: loc.clone(),
            comments: Vec::new(),
            commands: Vec::new(),
        };
        let paths = self.new_paths();
        let mut ds = Ds::new(stage, paths);
        ds.unregistered = true;
        self.add_state(ds)
    }

    fn build_stage_dependency_graph(&mut self) -> Result<(), Fail> {
        let mut i = 0;
        while i < self.states.len() {
            let commands = self
                .states
                .get(i)
                .map(|d| d.stage.commands.clone())
                .unwrap_or_default();
            let mut steps = Vec::with_capacity(commands.len());
            for c in commands {
                let loc = c.location.clone();
                // Its own stage is no dependency: what it declares comes first in it.
                let after = self.declaring_stage(&c).filter(|&s| s != i);
                let step = self.step_of(c)?;
                for &s in step.sources.iter().chain(&after) {
                    if let Some(d) = self.states.get_mut(i) {
                        set_dep(&mut d.deps, s, &loc);
                    }
                }
                steps.push(step);
            }
            if let Some(d) = self.states.get_mut(i) {
                d.steps = steps;
            }
            i += 1;
        }
        self.validate_circular_dependency()?;
        if self.states.len() == 1
            && let Some(d) = self.states.get_mut(0)
        {
            d.stage_name.clear();
        }
        Ok(())
    }

    /// `validateCircularDependency`: a stage that reads itself through others. Depth
    /// first, as BuildKit's recursion goes, on a stack of our own: a chain of stages as
    /// long as a Dockerfile can hold exhausts no thread's.
    fn validate_circular_dependency(&self) -> Result<(), Fail> {
        let n = self.states.len();
        let (mut visited, mut path) = (vec![false; n], vec![false; n]);
        // Each stage on the path, with the next of its dependencies to visit; `current`,
        // the steps that led to each but the first.
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut current: Vec<Location> = Vec::new();
        let mark = |marks: &mut Vec<bool>, i: usize, on: bool| {
            if let Some(m) = marks.get_mut(i) {
                *m = on;
            }
        };
        for start in 0..n {
            if visited.get(start).copied().unwrap_or(true) {
                continue;
            }
            mark(&mut visited, start, true);
            mark(&mut path, start, true);
            stack.push((start, 0));
            while let Some((state, next)) = stack.last_mut() {
                let deps = self
                    .states
                    .get(*state)
                    .map(|s| s.deps.as_slice())
                    .unwrap_or_default();
                let Some((dep, loc)) = deps.get(*next) else {
                    mark(&mut path, *state, false);
                    stack.pop();
                    current.pop();
                    continue;
                };
                *next += 1;
                current.push(loc.clone());
                if path.get(*dep).copied().unwrap_or(false) {
                    let name = self
                        .states
                        .get(start)
                        .map(|s| s.stage_name.clone())
                        .unwrap_or_default();
                    let mut f = Fail::new(errb(&[b"circular dependency detected on stage: ", &name]));
                    for loc in &current {
                        f = f.at(loc);
                    }
                    return Err(f);
                }
                if visited.get(*dep).copied().unwrap_or(true) {
                    current.pop();
                    continue;
                }
                mark(&mut visited, *dep, true);
                mark(&mut path, *dep, true);
                stack.push((*dep, 0));
            }
        }
        Ok(())
    }

    fn reachable(&self, from: usize) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(s) = stack.pop() {
            if !out.insert(s) {
                continue;
            }
            if let Some(d) = self.states.get(s) {
                if let Some(b) = d.base {
                    stack.push(b);
                }
                for (dep, _) in &d.deps {
                    stack.push(*dep);
                }
            }
        }
        out
    }

    /// `dispatchState.init`: a stage built on another starts from what that one made.
    fn init(&mut self, i: usize) {
        let Some(base) = self
            .states
            .get(i)
            .and_then(|d| d.base)
            .and_then(|b| self.states.get(b))
        else {
            return;
        };
        // What it takes of the base, and no more: the base's stage and steps stay.
        let (state, platform, mut image) = (base.state.clone(), base.platform.clone(), base.image.clone());
        image.config.on_build.clear();
        let (paths, workdir_set, build_args) = (base.paths, base.workdir_set, base.build_args.clone());
        let agentfile = base.agentfile.clone();
        let domains = base.domains.clone();
        if let Some(d) = self.states.get_mut(i) {
            d.domains = domains;
            d.state = state;
            d.platform = platform;
            d.image = image;
            d.paths = paths;
            d.workdir_set = workdir_set;
            d.build_args.extend(build_args);
            d.agentfile = agentfile;
        }
    }

    fn resolve_stages(&mut self, target: usize) -> Result<BTreeSet<usize>, Fail> {
        loop {
            let reachable = self.reachable(target);
            self.resolve_reachable_stages(&reachable)?;
            let mut new_deps = false;
            for &d in &reachable {
                self.init(d);
                let mut on_builds = self
                    .states
                    .get(d)
                    .map(|s| s.image.config.on_build.clone())
                    .unwrap_or_default();
                let base = self.states.get(d).and_then(|s| s.base);
                let init_done = self.states.get(d).is_some_and(|s| s.on_build_init);
                if let Some(b) = base
                    && !init_done
                {
                    let triggers: Vec<Vec<u8>> = self
                        .states
                        .get(b)
                        .map(|s| {
                            s.steps
                                .iter()
                                .filter_map(|st| match &st.command.kind {
                                    Kind::Onbuild(e) => Some(e.clone()),
                                    _ => None,
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    on_builds.extend(triggers);
                    if let Some(s) = self.states.get_mut(d) {
                        s.on_build_init = true;
                    }
                }
                if !on_builds.is_empty() {
                    let loc = self
                        .states
                        .get(d)
                        .map(|s| s.stage.location.clone())
                        .unwrap_or_default();
                    if self
                        .init_on_build_triggers(d, &on_builds)
                        .map_err(|f| f.set_at(&loc))?
                    {
                        new_deps = true;
                    }
                    if let Some(s) = self.states.get_mut(d) {
                        s.image.config.on_build.clear();
                    }
                }
            }
            if !new_deps {
                return Ok(reachable);
            }
        }
    }

    /// `initOnBuildTriggers`: the base's `ONBUILD` steps run first. Whether any reads
    /// another stage.
    fn init_on_build_triggers(&mut self, d: usize, triggers: &[Vec<u8>]) -> Result<bool, Fail> {
        let mut new_deps = false;
        let mut steps = Vec::new();
        let loc = self
            .states
            .get(d)
            .map(|s| s.stage.location.clone())
            .unwrap_or_default();
        let (start, end) = loc
            .iter()
            .fold((usize::MAX, 0), |(s, e), r| (s.min(r.0), e.max(r.1)));
        for t in triggers {
            let parsed = parser::parse(t).map_err(|e| Fail(e.message, e.location))?;
            let [node] = parsed.instructions.as_slice() else {
                return Err(Fail::new(
                    b"onbuild trigger should be a single expression".to_vec(),
                ));
            };
            let mut node = node.clone();
            if loc.is_empty() {
                node.start_line = 0;
                node.end_line = 0;
            } else {
                node.start_line = start;
                node.end_line = end;
            }
            let c = instructions::parse_command(&node).map_err(|e| Fail(e.message, e.location))?;
            let loc = c.location.clone();
            let after = self.declaring_stage(&c).filter(|&s| s != d);
            let mut step = self.step_of(c)?;
            step.on_build = true;
            if !step.sources.is_empty() || after.is_some() {
                new_deps = true;
            }
            if let Some(s) = self.states.get_mut(d) {
                for &src in step.sources.iter().chain(&after) {
                    set_dep(&mut s.deps, src, &loc);
                }
            }
            steps.push(step);
        }
        if let Some(s) = self.states.get_mut(d) {
            s.cmd_total += steps.len();
            steps.extend(std::mem::take(&mut s.steps));
            s.steps = steps;
        }
        Ok(new_deps)
    }

    fn resolve_reachable_stages(&mut self, reachable: &BTreeSet<usize>) -> Result<(), Fail> {
        for d in 0..self.states.len() {
            let Some(s) = self.states.get(d) else {
                continue;
            };
            if s.base.is_some() || s.dispatched || s.resolved {
                continue;
            }
            let is_reachable = self.opts.all_stages || reachable.contains(&d);
            let scratch = s.stage.base_name == EMPTY_IMAGE && s.named.is_none();
            let target = self.target_platform.clone();
            if let Some(s) = self.states.get_mut(d) {
                s.resolved = is_reachable;
                if scratch {
                    s.state = State::scratch();
                    s.image = empty_image(&target);
                    s.platform = Some(target);
                    if s.unregistered {
                        s.dispatched = true;
                    }
                    continue;
                }
            }
            let r = self.resolve_base_image(d, is_reachable);
            if let Some(s) = self.states.get_mut(d)
                && s.unregistered
            {
                s.dispatched = true;
            }
            let loc = self
                .states
                .get(d)
                .map(|s| s.stage.location.clone())
                .unwrap_or_default();
            r.map_err(|f| f.at(&loc))?;
        }
        Ok(())
    }

    fn resolve_base_image(&mut self, d: usize, reachable: bool) -> Result<(), Fail> {
        let Some(ds) = self.states.get(d) else {
            return Ok(());
        };
        let orig = ds.stage.base_name.clone();
        let mut r = parse_normalized(&orig).map_err(|e| {
            Fail::new(errb(&[
                b"failed to parse stage name ",
                go::quote(&orig).as_bytes(),
                b": ",
                &e,
            ]))
        })?;
        if r.tag.is_none() && r.digest.is_none() {
            r.tag = Some("latest".into());
        }
        let mut platform = ds
            .platform
            .clone()
            .unwrap_or_else(|| self.target_platform.clone());
        let mut base_name = r.to_string().into_bytes();
        let mut image = ds.image.clone();
        let mut scratch = false;
        let named = ds.named.clone();
        if reachable {
            // A stage that is a named context is that context, its steps not run.
            if let Some(n) = named {
                let (state, img) = self.load_named(d, &n, &platform)?;
                let ds = self.ds(d)?;
                ds.stage.base_name = base_name;
                ds.dispatched = true;
                ds.state = state;
                if let Some(mut img) = img {
                    img.created = None;
                    if !img.platform.architecture.is_empty() && !img.platform.os.is_empty() {
                        ds.platform = Some(Platform {
                            os: img.platform.os.clone(),
                            architecture: img.platform.architecture.clone(),
                            variant: img.platform.variant.clone(),
                            os_version: img.platform.os_version.clone(),
                            os_features: img.platform.os_features.clone(),
                        });
                    }
                    ds.image = img;
                }
                return Ok(());
            }
            // A base a named context names is that context.
            if let Some(n) = self.named_context(&base_name, Some(&platform))? {
                let (mut state, img) = self.load_named(d, &n, &platform)?;
                let mut img = img.unwrap_or_else(|| empty_image(&platform));
                img.created = None;
                state.platform = Some(platform.clone());
                let ds = self.ds(d)?;
                ds.stage.base_name = base_name;
                ds.image = img;
                ds.state = state;
                ds.platform = Some(platform);
                return Ok(());
            }
            let mut log = b"[".to_vec();
            if self.multi_platform {
                log.extend_from_slice(&platform::format_all(&platform));
                log.push(b' ');
            }
            log.extend_from_slice(b"internal] load metadata for ");
            log.extend_from_slice(&base_name);
            let resolved = self.resolver.resolve(&base_name, &platform, &log).map_err(|e| {
                let mut names = self.names();
                for n in [
                    "alpine", "busybox", "centos", "debian", "golang", "ubuntu", "fedora",
                ] {
                    names.push(n.as_bytes().to_vec());
                    // commonImageNames joins these without the slash, so that what it
                    // offers names no image: shards offers the name.
                    names.push(format!("docker.io/library/{n}").into_bytes());
                    names.push(format!("{n}:latest").into_bytes());
                    names.push(format!("docker.io/library/{n}:latest").into_bytes());
                }
                let options: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
                Fail::new(instructions::with_suggestion(
                    errb(&[&orig, b": ", &e]),
                    &orig,
                    &options,
                    true,
                ))
            })?;
            if resolved.reference != base_name {
                r = parse_normalized(&resolved.reference).map_err(|e| {
                    Fail::new(errb(&[
                        b"failed to parse ref ",
                        go::quote(&resolved.reference).as_bytes(),
                        b": ",
                        &e,
                    ]))
                })?;
            }
            let mut img = Image::from_json(&resolved.config)
                .map_err(|e| Fail::new(errb(&[b"failed to parse image config: ", &e])))?;
            img.created = None;
            if let Some(dg) = &resolved.digest {
                let s = std::str::from_utf8(dg).map_err(|_| Fail::new(b"invalid digest".to_vec()))?;
                r.digest = Some(
                    shards_image::reference::Digest::parse(s)
                        .map_err(|e| Fail::new(e.to_string().into_bytes()))?,
                );
            }
            base_name = r.to_string().into_bytes();
            if img.rootfs.diff_ids.as_ref().is_none_or(Vec::is_empty) {
                scratch = img.history.iter().all(|h| h.empty_layer);
            }
            image = img;
        }
        let multi = self.named_by_platform();
        let ds = self
            .states
            .get_mut(d)
            .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
        ds.stage.base_name = base_name.clone();
        ds.image = image;
        if scratch {
            ds.state = State::scratch();
        } else {
            let name = prefix_command(
                ds,
                &errb(&[b"FROM ", &base_name]),
                multi.as_ref(),
                Some(&platform),
                &EnvList::default(),
            );
            let mut meta = Meta::default();
            meta.description.insert(
                b"com.docker.dockerfile.v1.command".to_vec(),
                ds.stage.source_code.clone(),
            );
            meta.description.insert(b"llb.customname".to_vec(), name);
            meta.locations.push(ds.stage.location.clone());
            let mut attrs = BTreeMap::new();
            if !self.opts.image_resolve_mode.is_empty() {
                attrs.insert(
                    b"image.resolvemode".to_vec(),
                    self.opts.image_resolve_mode.clone(),
                );
            }
            ds.state = self.graph.source(
                [b"docker-image://".as_slice(), &base_name].concat(),
                attrs,
                Some(platform.clone()),
                meta,
            );
            if reachable {
                let actual = ds.image.platform.clone();
                if platform.os != actual.os || platform.architecture != actual.architecture {
                    let msg = errb(&[
                        b"Base image ",
                        &orig,
                        b" was pulled with platform ",
                        go::quote(&platform::format_all(&platform::normalize(&actual))).as_bytes(),
                        b", expected ",
                        go::quote(&platform::format_all(&platform::normalize(&platform))).as_bytes(),
                        b" for current build",
                    ]);
                    let loc = ds.stage.location.clone();
                    self.lint
                        .run(&lint::INVALID_BASE_IMAGE_PLATFORM, &loc, Some(&msg));
                }
            }
        }
        // The platform as given: Dockerfile2LLB is told the target, so it detects none.
        platform = platform.clone();
        if let Some(ds) = self.states.get_mut(d) {
            ds.platform = Some(platform);
        }
        Ok(())
    }

    fn dispatch_stages(&mut self, reachable: &BTreeSet<usize>, target: usize) -> Result<(), Fail> {
        if !self.opts.excludes.is_empty() {
            self.ignore = Some(crate::glob::PatternMatcher::new(&self.opts.excludes).map_err(Fail::new)?);
        }
        for d in 0..self.states.len() {
            // Every stage, with all stages, dispatched before or not (convert.go).
            if !self.opts.all_stages
                && (!reachable.contains(&d) || self.states.get(d).is_none_or(|s| s.dispatched))
            {
                continue;
            }
            self.init(d);
            let target_platform = self.target_platform.clone();
            let hostname = self.hostname.clone();
            {
                let ds = self
                    .states
                    .get_mut(d)
                    .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
                ds.dispatched = true;
                if ds.platform.is_none() {
                    ds.platform = Some(target_platform);
                }
                // PATH is always set.
                if !ds
                    .image
                    .config
                    .env
                    .iter()
                    .any(|e| parse_key_value(e).0 == b"PATH")
                {
                    ds.image
                        .config
                        .env
                        .push([b"PATH=".as_slice(), DEFAULT_PATH].concat());
                }
                ds.state
                    .env
                    .extend(ds.image.config.env.iter().map(|e| parse_key_value(e)));
                if !hostname.is_empty() {
                    ds.state.hostname = hostname;
                }
            }
            let (wd, user, loc) = {
                let ds = self
                    .states
                    .get(d)
                    .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
                (
                    ds.image.config.working_dir.clone(),
                    ds.image.config.user.clone(),
                    ds.stage.location.clone(),
                )
            };
            if !wd.is_empty() {
                self.dispatch_workdir(d, &wd, false, None)
                    .map_err(|f| f.at(&loc))?;
            }
            if !user.is_empty() {
                self.dispatch_user(d, &user, false);
            }
            let network = self.opts.network_mode;
            self.ds(d)?.state.network = network;
            let steps = self.states.get(d).map(|s| s.steps.clone()).unwrap_or_default();
            let first = self.graph.vertices.len();
            for step in steps {
                let loc = step.command.location.clone();
                self.dispatch(d, step).map_err(|f| f.at(&loc))?;
            }
            self.made.push((d, first..self.graph.vertices.len()));
        }
        let paths = self.states.get(target).map(|s| s.paths);
        if let Some(p) = paths.and_then(|p| self.path_sets.get_mut(p)) {
            p.insert(b"/".to_vec());
        }
        Ok(())
    }

    /// For a build whose steps are named by platform, the platform its frontend runs on.
    fn named_by_platform(&self) -> Option<Platform> {
        self.multi_platform
            .then(|| self.build_platforms.first().cloned().unwrap_or_default())
    }

    fn env_of(&self, d: usize) -> &EnvList {
        static NONE: EnvList = EnvList::new();
        self.states.get(d).map_or(&NONE, |s| &s.state.env)
    }

    /// `reportUnmatchedVariables`.
    fn report_unmatched(
        &self,
        d: usize,
        loc: &Location,
        env: &EnvList,
        unmatched: &BTreeSet<Vec<u8>>,
        lint: &LinterView<'_>,
    ) {
        if unmatched.is_empty() {
            return;
        }
        let args: BTreeSet<&[u8]> = self
            .states
            .get(d)
            .map(|s| s.build_args.iter().map(|a| a.key.as_slice()).collect())
            .unwrap_or_default();
        let keys: Vec<&[u8]> = env.keys().collect();
        for v in unmatched {
            if args.contains(v.as_slice()) || non_env_arg(v) {
                continue;
            }
            let mut msg = errb(&[b"Usage of undefined variable '$", v, b"'"]);
            if let Some(s) = instructions::suggest(v, &keys, true) {
                msg.extend_from_slice(&errb(&[b" (did you mean $", &s, b"?)"]));
            }
            lint.run(&lint::UNDEFINED_VAR, loc, Some(&msg));
        }
    }

    /// Expands `word` in stage `d`'s environment, reporting variables it does not define.
    fn expand(
        &self,
        d: usize,
        loc: &Location,
        lint: &LinterView<'_>,
        lexer: &Lex,
        word: &[u8],
    ) -> Result<Vec<u8>, Fail> {
        let env = self.env_of(d);
        let r = lexer.process(word, env);
        if let Ok(p) = &r {
            self.report_unmatched(d, loc, env, &p.unmatched, lint);
        }
        Ok(r?.word)
    }

    /// `dispatch`: one step.
    fn dispatch(&mut self, d: usize, mut step: Step) -> Result<(), Fail> {
        let lint = self.lint.with_comments(&step.command.comments);
        if let Some(s) = self.states.get_mut(d) {
            s.cmd_is_on_build = step.on_build;
        }
        let loc = step.command.location.clone();
        let shlex = self.shlex;
        let raw = Lex {
            skip_process_quotes: true,
            ..Lex::new(u32::from(b'\\'))
        };
        // Single words expand before the step; ARG expands its own.
        let ex = |w: &[u8]| self.expand(d, &loc, &lint, &shlex, w);
        match &mut step.command.kind {
            Kind::Env(kvs) | Kind::Label(kvs) => {
                for kv in kvs.iter_mut() {
                    kv.key = ex(&kv.key)?;
                    kv.value = ex(&kv.value)?;
                }
            }
            Kind::Add(a) => {
                a.chown = ex(&a.chown)?;
                a.chmod = ex(&a.chmod)?;
                a.checksum = ex(&a.checksum)?;
                for p in &mut a.sources.paths {
                    *p = ex(p)?;
                }
                a.sources.dest = ex(&a.sources.dest)?;
            }
            Kind::Copy(c) => {
                c.chown = ex(&c.chown)?;
                c.chmod = ex(&c.chmod)?;
                for p in &mut c.sources.paths {
                    *p = ex(p)?;
                }
                c.sources.dest = ex(&c.sources.dest)?;
            }
            Kind::Workdir(p) | Kind::User(p) | Kind::StopSignal(p) => *p = ex(p)?,
            Kind::Volume(v) => {
                for p in v.iter_mut() {
                    *p = ex(p)?;
                }
            }
            Kind::Agentfile(crate::agentfile::Directive::Volume(v)) => {
                for p in v.paths.iter_mut() {
                    *p = ex(p)?;
                }
                v.chown = ex(&v.chown)?;
                v.chmod = ex(&v.chmod)?;
            }
            // Their sources and paths as ADD's: names stay as written, as a stage's do.
            Kind::Agentfile(
                crate::agentfile::Directive::Agent(a) | crate::agentfile::Directive::Harness(a),
            ) => {
                a.source = ex(&a.source)?;
                if let Some(to) = &mut a.to {
                    *to = ex(to)?;
                }
            }
            Kind::Agentfile(crate::agentfile::Directive::Mcp(m)) => {
                m.source = ex(&m.source)?;
            }
            Kind::Agentfile(crate::agentfile::Directive::Skill(sk)) => {
                if let crate::agentfile::SkillSource::Path(p) = &mut sk.source {
                    *p = ex(p)?;
                }
                if let Some(dest) = &mut sk.dest {
                    *dest = ex(dest)?;
                }
                sk.chown = ex(&sk.chown)?;
                sk.chmod = ex(&sk.chmod)?;
                sk.checksum = ex(&sk.checksum)?;
            }
            Kind::Run(r) => {
                let mut mounts = Vec::new();
                for spec in &r.mount_specs {
                    let mut e = |w: &[u8]| ex(w).map_err(|f| f.0);
                    mounts.push(instructions::parse_mount(spec, Some(&mut e))?);
                }
                r.mounts = mounts;
            }
            _ => {}
        }
        // Heredoc contents expand raw.
        let ex_raw = |w: &[u8]| self.expand(d, &loc, &lint, &raw, w);
        match &mut step.command.kind {
            Kind::Add(a) => {
                for c in &mut a.sources.contents {
                    if c.expand {
                        c.data = ex_raw(&c.data)?;
                    }
                }
            }
            Kind::Copy(c) => {
                for s in &mut c.sources.contents {
                    if s.expand {
                        s.data = ex_raw(&s.data)?;
                    }
                }
            }
            _ => {}
        }
        let code = step.command.code.clone();
        let name = step.command.name.clone();
        match step.command.kind {
            Kind::Maintainer(m) => {
                let ds = self.ds(d)?;
                ds.image.author = m.clone();
                commit(ds, errb(&[b"MAINTAINER ", &m]), false, false);
            }
            Kind::Env(kvs) => {
                let ds = self.ds(d)?;
                let mut msg = b"ENV".to_vec();
                for kv in &kvs {
                    if kv.no_delim {
                        lint.run(&lint::LEGACY_KEY_VALUE_FORMAT, &loc, Some(&legacy_kv(&name)));
                    }
                    validate_no_secret_key(b"ENV", &kv.key, &loc, &lint);
                    msg.extend_from_slice(&errb(&[b" ", &kv.key, b"=", &kv.value]));
                    ds.state.env.add(&kv.key, &kv.value);
                    add_env(&mut ds.image.config.env, &kv.key, &kv.value);
                }
                commit(ds, msg, false, false);
            }
            Kind::Label(kvs) => {
                let ds = self.ds(d)?;
                let mut msg = b"LABEL".to_vec();
                for kv in &kvs {
                    if kv.no_delim {
                        lint.run(&lint::LEGACY_KEY_VALUE_FORMAT, &loc, Some(&legacy_kv(&name)));
                    }
                    ds.image.config.labels.insert(kv.key.clone(), kv.value.clone());
                    msg.extend_from_slice(&errb(&[b" ", &kv.key, b"=", &kv.value]));
                }
                commit(ds, msg, false, false);
            }
            Kind::Onbuild(e) => {
                self.ds(d)?.image.config.on_build.push(e);
            }
            Kind::Cmd(c) => {
                let ds = self.ds(d)?;
                validate_used_once(&mut ds.cmd, &name, &loc, &lint);
                let mut args = c.cmd_line.clone();
                if c.prepend_shell {
                    if ds.image.config.shell.is_empty() {
                        lint.run(&lint::JSON_ARGS_RECOMMENDED, &loc, Some(&json_args(&name)));
                    }
                    args = with_shell(&ds.image, &args);
                }
                ds.image.config.cmd = args.clone();
                ds.image.config.args_escaped = true;
                commit(ds, errb(&[b"CMD ", &quote_list(&args)]), false, false);
            }
            Kind::Entrypoint(c) => {
                let ds = self.ds(d)?;
                validate_used_once(&mut ds.entrypoint, &name, &loc, &lint);
                let mut args = c.cmd_line.clone();
                if c.prepend_shell {
                    if ds.image.config.shell.is_empty() {
                        lint.run(&lint::JSON_ARGS_RECOMMENDED, &loc, Some(&json_args(&name)));
                    }
                    args = with_shell(&ds.image, &args);
                }
                ds.image.config.entrypoint = args.clone();
                if ds.cmd.0.is_none() {
                    ds.image.config.cmd.clear();
                }
                commit(ds, errb(&[b"ENTRYPOINT ", &quote_list(&args)]), false, false);
            }
            Kind::Healthcheck(h) => {
                let ds = self.ds(d)?;
                validate_used_once(&mut ds.healthcheck, &name, &loc, &lint);
                let hc = crate::image::Healthcheck {
                    test: h.test.clone(),
                    interval: h.interval,
                    timeout: h.timeout,
                    start_period: h.start_period,
                    start_interval: h.start_interval,
                    retries: h.retries,
                };
                let msg = errb(&[b"HEALTHCHECK ", &healthcheck_string(&hc)]);
                ds.image.config.healthcheck = Some(hc);
                commit(ds, msg, false, false);
            }
            Kind::Expose(ports) => self.dispatch_expose(d, &ports, &loc, &lint)?,
            // What a Dockerfile's engine sees of an Agentfile's ports and volumes: the ports
            // it listens on, egress ones not (§12.6), and the mount points. Their direction,
            // networks, names and grants go in the normalized Agentfile (§8).
            Kind::Agentfile(crate::agentfile::Directive::Expose(e)) => {
                if e.direction != crate::agentfile::Direction::Egress {
                    self.dispatch_expose(d, &e.ports, &loc, &lint)?;
                }
                self.ds(d)?.agentfile.push(crate::agentfile::Directive::Expose(e));
            }
            Kind::Agentfile(crate::agentfile::Directive::Volume(v)) => {
                let ds = self.ds(d)?;
                for p in &v.paths {
                    if p.is_empty() {
                        return Err(Fail::new(b"VOLUME specified can not be an empty string".to_vec()));
                    }
                    ds.image.config.volumes.insert(p.clone(), ());
                }
                commit(ds, errb(&[b"VOLUME ", &go_list(&v.paths)]), false, false);
                ds.agentfile.push(crate::agentfile::Directive::Volume(v));
            }
            // What declares and grants alone: the normalized Agentfile carries it.
            Kind::Agentfile(
                directive @ (crate::agentfile::Directive::Network(_)
                | crate::agentfile::Directive::Connect(_)
                | crate::agentfile::Directive::Attach(_)),
            ) => self.ds(d)?.agentfile.push(directive),
            Kind::Agentfile(crate::agentfile::Directive::Agent(a)) => {
                let dest = domain_dir(&a, false).map_err(Fail::new)?;
                self.ds(d)?.domains.push(DomainDir {
                    name: a.name.clone(),
                    harness: false,
                    dir: dest.clone(),
                });
                self.domain_sources
                    .entry(a.name.clone())
                    .or_insert_with(|| (b"agent".to_vec(), a.source.clone()));
                self.fetch_into(d, b"agent", &a.source, &dest, &code, &loc, &lint)?;
                self.ds(d)?.agentfile.push(crate::agentfile::Directive::Agent(a));
            }
            Kind::Agentfile(crate::agentfile::Directive::Harness(h)) => {
                let dest = domain_dir(&h, true).map_err(Fail::new)?;
                self.ds(d)?.domains.push(DomainDir {
                    name: h.name.clone(),
                    harness: true,
                    dir: dest.clone(),
                });
                self.domain_sources
                    .entry(h.name.clone())
                    .or_insert_with(|| (b"harness".to_vec(), h.source.clone()));
                self.fetch_into(d, b"harness", &h.source, &dest, &code, &loc, &lint)?;
                self.ds(d)?
                    .agentfile
                    .push(crate::agentfile::Directive::Harness(h));
            }
            Kind::Agentfile(crate::agentfile::Directive::Mcp(m)) => {
                // A remote server is reached, not fetched (§4.4); one over stdio is laid out
                // where its scope says (§12.1).
                let remote = matches!(
                    crate::agentfile::source_of(&m.source).map_err(Fail::new)?,
                    crate::agentfile::Source::Http(_)
                );
                if !remote {
                    let dests = if m.scope.names.is_empty() {
                        vec![errb(&[b"/mcp/", &m.name])]
                    } else {
                        self.grant_dirs(d, &m.scope, b"mcp")?
                            .into_iter()
                            .map(|g| errb(&[&g, b"/", &m.name]))
                            .collect()
                    };
                    for dest in dests {
                        self.fetch_into(d, b"mcp", &m.source, &dest, &code, &loc, &lint)?;
                    }
                }
                self.ds(d)?.agentfile.push(crate::agentfile::Directive::Mcp(m));
            }
            Kind::Agentfile(crate::agentfile::Directive::Skill(sk)) => {
                let domain = if !sk.from.is_empty() && self.declares_domain(&sk.from) {
                    Some(self.domain_skill(d, &sk)?)
                } else {
                    None
                };
                self.dispatch_skill(d, &sk, &step.sources, domain, &code, &loc, &lint)?;
                self.ds(d)?.agentfile.push(crate::agentfile::Directive::Skill(sk));
            }
            Kind::User(u) => self.dispatch_user(d, &u, true),
            Kind::Volume(v) => {
                let ds = self.ds(d)?;
                for p in &v {
                    if p.is_empty() {
                        return Err(Fail::new(b"VOLUME specified can not be an empty string".to_vec()));
                    }
                    ds.image.config.volumes.insert(p.clone(), ());
                }
                commit(ds, errb(&[b"VOLUME ", &go_list(&v)]), false, false);
            }
            Kind::StopSignal(s) => {
                parse_signal(&s)?;
                let ds = self.ds(d)?;
                ds.image.config.stop_signal = s.clone();
                commit(ds, errb(&[b"STOPSIGNAL ", &s]), false, false);
            }
            Kind::Shell(sh) => {
                let ds = self.ds(d)?;
                ds.image.config.shell = sh.clone();
                commit(ds, errb(&[b"SHELL ", &go_list(&sh)]), false, false);
            }
            Kind::Arg(defs) => self.dispatch_arg(d, defs, &loc, &lint)?,
            Kind::Workdir(p) => self.dispatch_workdir(d, &p, true, Some((&code, &loc, &lint)))?,
            Kind::Run(r) => self.dispatch_run(d, r, &step.sources, &code, &loc)?,
            Kind::Add(a) => {
                let cfg = CopyConfig {
                    sources: a.sources.clone(),
                    exclude: a.exclude.clone(),
                    from: None,
                    is_add: true,
                    code: code.clone(),
                    chown: a.chown.clone(),
                    chmod: a.chmod.clone(),
                    link: a.link,
                    keep_git_dir: a.keep_git_dir,
                    checksum: a.checksum.clone(),
                    parents: false,
                    unpack: a.unpack,
                    onto: None,
                    history: None,
                    location: loc.clone(),
                };
                self.dispatch_copy(d, cfg, &loc, &lint)?;
                let ds = self.ds(d)?;
                for src in &a.sources.paths {
                    if !src.starts_with(b"http://") && !src.starts_with(b"https://") {
                        ds.ctx_paths.insert(go::join(&[b"/", src]));
                    }
                }
            }
            Kind::Copy(c) => {
                let from = match step.sources.first() {
                    Some(&s) => {
                        let src = self
                            .states
                            .get(s)
                            .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
                        if !src.dispatched {
                            let cur = self
                                .states
                                .get(d)
                                .map(|x| x.stage_name.clone())
                                .unwrap_or_default();
                            return Err(Fail::new(errb(&[
                                b"cannot copy from stage ",
                                go::quote(&c.from).as_bytes(),
                                b", it needs to be defined before current stage ",
                                go::quote(&cur).as_bytes(),
                            ])));
                        }
                        Some(src.state.clone())
                    }
                    None if !c.from.is_empty() && self.declares_domain(&c.from) => {
                        Some(self.domain_content(d, &c.from)?)
                    }
                    None => None,
                };
                let from_domain = step.sources.is_empty() && !c.from.is_empty();
                let cfg = CopyConfig {
                    sources: c.sources.clone(),
                    exclude: c.exclude.clone(),
                    from,
                    is_add: false,
                    code: code.clone(),
                    chown: c.chown.clone(),
                    chmod: c.chmod.clone(),
                    link: c.link,
                    keep_git_dir: None,
                    checksum: Vec::new(),
                    parents: c.parents,
                    unpack: None,
                    onto: None,
                    history: None,
                    location: loc.clone(),
                };
                self.dispatch_copy(d, cfg, &loc, &lint)?;
                match step.sources.first() {
                    None if from_domain => {}
                    None => {
                        let ds = self.ds(d)?;
                        for src in &c.sources.paths {
                            ds.ctx_paths.insert(go::join(&[b"/", src]));
                        }
                    }
                    Some(&s) => {
                        let set = self.states.get(s).map(|x| x.paths);
                        if let Some(set) = set.and_then(|p| self.path_sets.get_mut(p)) {
                            for src in &c.sources.paths {
                                set.insert(go::join(&[b"/", src]));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn ds(&mut self, d: usize) -> Result<&mut Ds, Fail> {
        self.states
            .get_mut(d)
            .ok_or_else(|| Fail::new(b"no stage".to_vec()))
    }

    fn dispatch_user(&mut self, d: usize, user: &[u8], commit_it: bool) {
        if let Some(ds) = self.states.get_mut(d) {
            ds.state.user = user.to_vec();
            ds.image.config.user = user.to_vec();
            if commit_it {
                commit(ds, errb(&[b"USER ", user]), false, false);
            }
        }
    }

    fn dispatch_workdir(
        &mut self,
        d: usize,
        path: &[u8],
        commit_it: bool,
        cmd: Option<(&[u8], &Location, &LinterView<'_>)>,
    ) -> Result<(), Fail> {
        let multi = self.named_by_platform();
        let epoch = self.epoch;
        let ds = self.ds(d)?;
        if commit_it {
            if !ds.workdir_set
                && !go::is_abs(path)
                && let Some((_, loc, lint)) = cmd
            {
                let msg = errb(&[
                    b"Relative workdir ",
                    go::quote(path).as_bytes(),
                    b" can have unexpected results if the base image changes",
                ]);
                lint.run(&lint::WORKDIR_RELATIVE_PATH, loc, Some(&msg));
            }
            ds.workdir_set = true;
        }
        let wd = normalize_path(&ds.image.config.working_dir, path, false);
        ds.image.config.working_dir = wd.clone();
        ds.state.set_dir(&wd);
        if !commit_it {
            return Ok(());
        }
        let mut with_layer = false;
        if wd != b"/" {
            let Some((code, _, _)) = cmd else {
                return Ok(());
            };
            let env = ds.state.env.clone();
            let lexer = self.shlex;
            let shown = uppercase_cmd(&process_cmd_env(&lexer, code, &env));
            let ds = self.ds(d)?;
            let platform = ds.platform.clone();
            let name = prefix_command(ds, &shown, multi.as_ref(), platform.as_ref(), &env);
            let chown = (!ds.image.config.user.is_empty()).then(|| Chown::parse(&ds.image.config.user));
            let action = Action::Mkdir {
                path: wd.clone(),
                mode: 0o755,
                make_parents: true,
                chown,
                // UnixNano, which wraps where Go's does.
                created: epoch.map(|(s, ns)| s.wrapping_mul(1_000_000_000).wrapping_add(i64::from(ns))),
            };
            let state = ds.state.clone();
            let mut meta = custom_name(name);
            if let Some((_, loc, _)) = cmd {
                meta.locations.push(loc.clone());
            }
            let next = self.graph.file(&state, vec![action], meta);
            let ds = self.ds(d)?;
            ds.state.output = next.output;
            with_layer = true;
        }
        let ds = self.ds(d)?;
        commit(ds, errb(&[b"WORKDIR ", &wd]), with_layer, false);
        Ok(())
    }

    fn dispatch_arg(
        &mut self,
        d: usize,
        defs: Vec<ArgDef>,
        loc: &Location,
        lint: &LinterView<'_>,
    ) -> Result<(), Fail> {
        let mut commits = Vec::new();
        for (seq, mut arg) in defs.into_iter().enumerate() {
            validate_no_secret_key(b"ARG", &arg.key, loc, lint);
            let has_value = self.build_args.get(&arg.key).cloned();
            let has_default = arg.value.is_some();
            // Inherited from the global scope, whose outline entry it keeps (skipArgInfo).
            let mut inherited = false;
            if !has_default
                && has_value.is_none()
                && let Some(v) = self.global_args.get(&arg.key)
            {
                arg.value = Some(v.to_vec());
                inherited = true;
            }
            if let Some(v) = has_value {
                arg.value = Some(v);
            } else if has_default {
                let v = arg.value.clone().unwrap_or_default();
                let shlex = self.shlex;
                arg.value = Some(self.expand(d, loc, lint, &shlex, &v)?);
            }
            let ds = self.ds(d)?;
            if let Some(v) = &arg.value
                && !non_env_arg(&arg.key)
            {
                ds.state.env.add(&arg.key, v);
            }
            if !inherited {
                ds.outline.all_args.insert(
                    arg.key.clone(),
                    ArgInfo {
                        key: arg.key.clone(),
                        doc: arg.doc_comment.clone(),
                        value: arg.value.clone(),
                        deps: BTreeSet::new(),
                        location: loc.clone(),
                        seq,
                    },
                );
            }
            ds.outline.used.insert(arg.key.clone());
            let mut c = arg.key.clone();
            if let Some(v) = &arg.value {
                c.push(b'=');
                c.extend_from_slice(v);
            }
            commits.push(c);
            ds.build_args.push(arg);
        }
        let ds = self.ds(d)?;
        commit(ds, errb(&[b"ARG ", &commits.join(&b' ')]), false, false);
        Ok(())
    }

    fn dispatch_expose(
        &mut self,
        d: usize,
        ports: &[Vec<u8>],
        loc: &Location,
        lint: &LinterView<'_>,
    ) -> Result<(), Fail> {
        let mut words = Vec::new();
        for p in ports {
            words.extend(self.shlex.process(p, self.env_of(d))?.words);
        }
        let mut exposed = Vec::new();
        for raw in &words {
            exposed.extend(parse_port(raw, loc, lint)?);
        }
        let ds = self.ds(d)?;
        for p in &exposed {
            ds.image.config.exposed_ports.insert(p.clone(), ());
        }
        commit(ds, errb(&[b"EXPOSE ", &go_list(&exposed)]), false, false);
        Ok(())
    }

    fn dispatch_run(
        &mut self,
        d: usize,
        r: instructions::Run,
        sources: &[usize],
        code: &[u8],
        loc: &Location,
    ) -> Result<(), Fail> {
        let multi = self.named_by_platform();
        let paths = self.ds(d)?.paths;
        if let Some(set) = self.path_sets.get_mut(paths) {
            set.insert(b"/".to_vec());
        }
        let mut custom = code.to_vec();
        let mut args = r.cmd.cmd_line.clone();
        let mut extra_mounts = Vec::new();
        let ds = self
            .states
            .get(d)
            .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
        if !r.cmd.files.is_empty() {
            let [first_arg] = args.as_slice() else {
                return Err(Fail::new(errb(&[
                    b"parsing produced an invalid run command: ",
                    &go_list(&args),
                ])));
            };
            if !r.cmd.prepend_shell {
                return Err(Fail::new(errb(&[
                    b"parsing produced an invalid run command: ",
                    &go_list(&args),
                ])));
            }
            if parser::heredoc_word(first_arg).is_some() {
                let file = r
                    .cmd
                    .files
                    .first()
                    .ok_or_else(|| Fail::new(b"no heredoc".to_vec()))?;
                let mut data = file.data.clone();
                if file.chomp {
                    data = chomp_heredoc(&data);
                }
                if ds.image.platform.os != b"windows" && data.starts_with(b"#!") {
                    let platform = ds.platform.clone();
                    let mut st = State::scratch();
                    st.set_dir(b"/");
                    let action = Action::Mkfile {
                        path: file.name.clone(),
                        mode: 0o755,
                        data,
                        chown: None,
                        created: None,
                    };
                    st.platform = platform;
                    let mut meta = custom_name(b"[internal] preparing inline document".to_vec());
                    meta.progress_group = None;
                    let made = self.graph.file(&st, vec![action], meta);
                    extra_mounts.push(Mount {
                        target: b"/dev/pipes/".to_vec(),
                        source: made.output,
                        readonly: true,
                        selector: b"/".to_vec(),
                        kind: MountKind::Bind,
                        no_output: false,
                    });
                    args = vec![go::join(&[b"/dev/pipes/", &file.name])];
                } else {
                    args = vec![data];
                }
                custom.extend_from_slice(&errb(&[b" (", &summarize_heredoc(&file.data), b")"]));
            } else {
                let mut b = first_arg.clone();
                for f in &r.cmd.files {
                    b.push(b'\n');
                    b.extend_from_slice(&f.data);
                    b.extend_from_slice(&f.name);
                }
                args = vec![b];
            }
        }
        let ds = self
            .states
            .get(d)
            .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
        if r.cmd.prepend_shell {
            args = with_shell(&ds.image, &args);
        }
        let mut run = llb::Run {
            args: args.clone(),
            proxy: self.proxy.clone(),
            ..llb::Run::default()
        };
        run.meta
            .description
            .insert(b"com.docker.dockerfile.v1.command".to_vec(), code.to_vec());
        run.mounts.extend(extra_mounts);
        self.dispatch_run_mounts(d, &r, sources, &mut run, loc)?;
        run.security = Some(match r.security.as_slice() {
            b"insecure" => Security::Insecure,
            b"sandbox" => Security::Sandbox,
            s => {
                return Err(Fail::new(errb(&[
                    b"unsupported security mode ",
                    go::quote(s).as_bytes(),
                ])));
            }
        });
        match r.network.as_slice() {
            b"default" => {}
            b"none" => run.network = Some(NetMode::None),
            b"host" => run.network = Some(NetMode::Host),
            n => {
                return Err(Fail::new(errb(&[
                    b"unsupported network mode ",
                    go::quote(n).as_bytes(),
                ])));
            }
        }
        for dev in &r.devices {
            run.devices.push(llb::Device {
                name: dev.name.clone(),
                optional: !dev.required,
            });
        }
        // The progress name: the command with variables expanded, secrets masked, and
        // unset ones left as written.
        let ds = self
            .states
            .get(d)
            .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
        let env = ds.state.env.clone();
        let mut shown_env = env.clone();
        for m in &r.mounts {
            if m.kind == b"secret"
                && let Some(e) = &m.env
            {
                shown_env.add(e, b"****");
            }
        }
        let lexer = Lex {
            raw_quotes: true,
            skip_unset_env: true,
            ..self.shlex
        };
        let shown = uppercase_cmd(&process_cmd_env(&lexer, &custom, &shown_env));
        let platform = ds.state.platform.clone();
        let opts = self.opts;
        let ds = self.ds(d)?;
        let name = prefix_command(ds, &shown, multi.as_ref(), platform.as_ref(), &env);
        run.meta.description.insert(b"llb.customname".to_vec(), name);
        run.meta.locations.push(loc.clone());
        run.meta.ignore_cache = ds.ignore_cache;
        // AddUlimit, AddExtraHost and WithCgroupParent, options of the run alone
        // (dispatchRun): the stage's next state is its root mount's, which keeps none.
        let mut state = ds.state.clone();
        let kept = (
            state.ulimits.clone(),
            state.extra_hosts.clone(),
            state.cgroup_parent.clone(),
        );
        state.ulimits.extend(opts.ulimits.iter().cloned());
        state.extra_hosts.extend(opts.extra_hosts.iter().cloned());
        if !opts.cgroup_parent.is_empty() {
            state.cgroup_parent.clone_from(&opts.cgroup_parent);
        }
        if opts.shm_size > 0 {
            run.mounts.push(Mount {
                target: b"/dev/shm".to_vec(),
                source: None,
                readonly: false,
                selector: Vec::new(),
                kind: MountKind::Tmpfs { size: opts.shm_size },
                no_output: false,
            });
        }
        if opts.linux_resources.is_some() {
            run.meta.linux_resources.clone_from(&opts.linux_resources);
        }
        let mut next = self.graph.run(&state, run);
        (next.ulimits, next.extra_hosts, next.cgroup_parent) = kept;
        let build_args = self
            .states
            .get(d)
            .map(|s| s.build_args.clone())
            .unwrap_or_default();
        let msg = errb(&[b"RUN ", &run_command_string(&args, &build_args, &env)]);
        let ds = self.ds(d)?;
        ds.state = next;
        commit(ds, msg, true, true);
        Ok(())
    }

    fn dispatch_run_mounts(
        &mut self,
        d: usize,
        r: &instructions::Run,
        sources: &[usize],
        run: &mut llb::Run,
        loc: &Location,
    ) -> Result<(), Fail> {
        for (i, m0) in r.mounts.iter().enumerate() {
            let mut m = m0.clone();
            if m.from.is_empty() && m.kind == b"cache" {
                m.from = EMPTY_IMAGE.to_vec();
            }
            let mut source: Option<Output> = Some(self.context);
            if !m.from.is_empty() {
                let s = sources
                    .get(i)
                    .copied()
                    .ok_or_else(|| Fail::new(b"no mount source".to_vec()))?;
                let src = self
                    .states
                    .get(s)
                    .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
                if !src.dispatched {
                    return Err(Fail::new(errb(&[
                        b"cannot mount from stage ",
                        go::quote(&m.from).as_bytes(),
                        b" to ",
                        go::quote(&m.target).as_bytes(),
                        b", stage needs to be defined before current command",
                    ])));
                }
                source = src.state.output;
            }
            match m.kind.as_slice() {
                b"secret" => {
                    let secret = dispatch_secret(&m)?;
                    // The outline's: where each is first named (dispatchSecret).
                    self.ds(d)?.outline.secrets.entry(secret.id.clone()).or_insert((
                        m.required,
                        loc.clone(),
                        i,
                    ));
                    run.secrets.push(secret);
                    continue;
                }
                b"ssh" => {
                    let ssh = dispatch_ssh(&m)?;
                    let id = if m.id.is_empty() {
                        b"default".to_vec()
                    } else {
                        m.id.clone()
                    };
                    self.ds(d)?
                        .outline
                        .ssh
                        .entry(id)
                        .or_insert((m.required, loc.clone(), i));
                    run.ssh.push(ssh);
                    continue;
                }
                _ => {}
            }
            let mut kind = MountKind::Bind;
            let mut no_output = false;
            if m.kind == b"tmpfs" {
                source = None;
                kind = MountKind::Tmpfs { size: m.size };
            }
            if !m.read_only && m.kind == b"bind" {
                // The bind's changes are dropped: CapExecMountBindReadWriteNoOutput.
                no_output = true;
            }
            if m.kind == b"cache" {
                let sharing = match m.sharing.as_slice() {
                    b"private" => Sharing::Private,
                    b"locked" => Sharing::Locked,
                    _ => Sharing::Shared,
                };
                if m.id.is_empty() {
                    m.id = go::clean(&m.target);
                }
                kind = MountKind::Cache {
                    id: errb(&[&self.cache_ns, b"/", &m.id]),
                    sharing,
                };
            }
            let mut target = m.target.clone();
            if !go::is_abs(&go::clean(&m.target)) {
                let dir = self
                    .states
                    .get(d)
                    .map(|s| s.state.dir.clone())
                    .unwrap_or_default();
                target = go::join(&[b"/", &dir, &m.target]);
            }
            if target == b"/" {
                return Err(Fail::new(errb(&[
                    b"invalid mount target ",
                    go::quote(&target).as_bytes(),
                ])));
            }
            let mut selector = Vec::new();
            let src_path = go::join(&[b"/", &m.source]);
            if src_path != b"/" {
                selector = src_path;
            } else if m.uid.is_some() || m.gid.is_some() || m.mode.is_some() {
                // setCacheUIDGID: a directory made with the owner and mode asked.
                let mut base = State::scratch();
                base.output = source;
                let action = Action::Mkdir {
                    path: b"/cache".to_vec(),
                    mode: u32::try_from(m.mode.unwrap_or(0o755)).unwrap_or(0o755),
                    make_parents: false,
                    chown: Some(Chown {
                        user: Some(llb::UserOpt::Id(u32::try_from(m.uid.unwrap_or(0)).unwrap_or(0))),
                        group: Some(llb::UserOpt::Id(u32::try_from(m.gid.unwrap_or(0)).unwrap_or(0))),
                    }),
                    created: None,
                };
                let made = self.graph.file(
                    &base,
                    vec![action],
                    custom_name(b"[internal] setting cache mount permissions".to_vec()),
                );
                source = made.output;
                selector = b"/cache".to_vec();
            }
            run.mounts.push(Mount {
                target,
                source,
                readonly: m.read_only,
                selector,
                kind,
                no_output,
            });
            let used = go::join(&[b"/", &m.source]);
            if m.from.is_empty() {
                self.ds(d)?.ctx_paths.insert(used);
            } else if let Some(&s) = sources.get(i) {
                let set = self.states.get(s).map(|x| x.paths);
                if let Some(set) = set.and_then(|p| self.path_sets.get_mut(p)) {
                    set.insert(used);
                }
            }
        }
        Ok(())
    }

    fn dispatch_copy(
        &mut self,
        d: usize,
        cfg: CopyConfig,
        loc: &Location,
        lint: &LinterView<'_>,
    ) -> Result<Option<State>, Fail> {
        let multi = self.named_by_platform();
        let target_platform = self.target_platform.clone();
        let ds = self
            .states
            .get(d)
            .ok_or_else(|| Fail::new(b"no stage".to_vec()))?;
        let dest = path_relative_to_working_dir(&ds.state.dir, &cfg.sources.dest);
        let chown = (!cfg.chown.is_empty()).then(|| Chown::parse(&cfg.chown));
        let mode = parse_chmod(&cfg.chmod)?;
        if !cfg.checksum.is_empty() {
            if !cfg.is_add {
                return Err(Fail::new(b"checksum can't be specified for COPY".to_vec()));
            }
            let [only] = cfg.sources.paths.as_slice() else {
                return Err(Fail::new(
                    b"checksum can't be specified for multiple sources".to_vec(),
                ));
            };
            if !is_http_source(only) && !matches!(git::parse_git_ref(only), git::Parsed::Git(_)) {
                return Err(Fail::new(b"checksum requires HTTP(S) or Git sources".to_vec()));
            }
        }
        let mut checksum = cfg.checksum.clone();
        let mut keep_git_dir = cfg.keep_git_dir;
        let mut msg: Vec<u8> = if cfg.is_add {
            b"ADD".to_vec()
        } else {
            b"COPY".to_vec()
        };
        if cfg.parents {
            msg.extend_from_slice(b" --parents");
        }
        if !cfg.chown.is_empty() {
            msg.extend_from_slice(&errb(&[b" --chown=", &cfg.chown]));
        }
        if !cfg.chmod.is_empty() {
            msg.extend_from_slice(&errb(&[b" --chmod=", &cfg.chmod]));
        }
        let platform = ds.platform.clone().unwrap_or(target_platform);
        let env = ds.state.env.clone();
        let name = uppercase_cmd(&process_cmd_env(&self.shlex, &cfg.code, &env));
        let ds = self.ds(d)?;
        let pg_name = prefix_command(ds, &name, multi.as_ref(), Some(&platform), &env);
        let mut actions = Vec::new();
        let source_state = cfg.from.clone().unwrap_or_else(|| {
            let mut s = State::scratch();
            s.output = Some(self.context);
            s
        });
        for src in &cfg.sources.paths {
            msg.extend_from_slice(&errb(&[b" ", src]));
            let git_ref = match git::parse_git_ref(src) {
                git::Parsed::BadGit(e) => return Err(Fail::new(e)),
                git::Parsed::Git(g) if !g.indistinguishable_from_local => Some(g),
                _ => None,
            };
            if let Some(g) = git_ref {
                if !cfg.is_add {
                    return Err(Fail::new(b"source can't be a git ref for COPY".to_vec()));
                }
                if let (Some(a), Some(b)) = (keep_git_dir, g.keep_git_dir)
                    && a != b
                {
                    return Err(Fail::new(b"inconsistent keep-git-dir configuration".to_vec()));
                }
                if g.keep_git_dir.is_some() {
                    keep_git_dir = g.keep_git_dir;
                }
                if !checksum.is_empty() && !g.checksum.is_empty() && checksum != g.checksum {
                    return Err(Fail::new(errb(&[
                        b"checksum mismatch ",
                        go::quote(&checksum).as_bytes(),
                        b" != ",
                        go::quote(&g.checksum).as_bytes(),
                    ])));
                }
                if !g.checksum.is_empty() {
                    checksum = g.checksum.clone();
                }
                let st = self.git_source(&g, keep_git_dir == Some(true), &checksum, &pg_name);
                actions.push(Action::Copy {
                    from: st.output,
                    from_dir: st.dir.clone(),
                    src: b"/".to_vec(),
                    dest: dest.clone(),
                    info: CopyInfo {
                        mode: mode.clone(),
                        create_dest_path: true,
                        exclude_patterns: cfg.exclude.clone(),
                        chown: chown.clone(),
                        ..CopyInfo::default()
                    },
                });
                continue;
            }
            if is_http_source(src) {
                if !cfg.is_add {
                    return Err(Fail::new(b"source can't be a URL for COPY".to_vec()));
                }
                // Not unpacked unless asked: remote archives stay as they are.
                let name = http_filename(src);
                let mut attrs = BTreeMap::new();
                if !checksum.is_empty() {
                    attrs.insert(b"http.checksum".to_vec(), parse_checksum(&checksum)?);
                }
                attrs.insert(b"http.filename".to_vec(), name.clone());
                let mut meta = custom_name(pg_name.clone());
                // dfCmd of the sources: they print nothing.
                meta.description
                    .insert(b"com.docker.dockerfile.v1.command".to_vec(), Vec::new());
                let st = self.graph.source(src.clone(), attrs, None, meta);
                actions.push(Action::Copy {
                    from: st.output,
                    from_dir: st.dir.clone(),
                    src: name,
                    dest: dest.clone(),
                    info: CopyInfo {
                        mode: mode.clone(),
                        create_dest_path: true,
                        attempt_unpack: cfg.unpack.unwrap_or(false),
                        exclude_patterns: cfg.exclude.clone(),
                        chown: chown.clone(),
                        ..CopyInfo::default()
                    },
                });
                continue;
            }
            // What ADD reads, and COPY from the context, is held to the .dockerignore.
            if cfg.from.is_none()
                && let Some(ignore) = self.ignore.as_mut()
            {
                validate_copy_source_path(ignore, src, cfg.is_add, loc, lint);
            }
            let (mut src, mut patterns, mut required) = (src.clone(), Vec::new(), Vec::new());
            if cfg.parents {
                let (parent, pattern) = match find(&src, b"/./") {
                    Some(at) => (go::head(&src, at).to_vec(), go::tail(&src, at + 3).to_vec()),
                    None => (b"/".to_vec(), src.clone()),
                };
                let pattern = normalize_path(b"/", &pattern, false);
                let trimmed = pattern.strip_prefix(b"/").unwrap_or(&pattern).to_vec();
                if !contains_wildcards(&parent) && !contains_wildcards(&pattern) {
                    required.push(normalize_path(b"/", &filepath_join(&parent, &pattern), false));
                }
                patterns.push(trimmed);
                src = parent;
            }
            let src = normalize_path(b"/", &src, false);
            let unpack = cfg.unpack.unwrap_or(cfg.is_add);
            actions.push(Action::Copy {
                from: source_state.output,
                from_dir: source_state.dir.clone(),
                src,
                dest: dest.clone(),
                info: CopyInfo {
                    mode: mode.clone(),
                    follow_symlinks: true,
                    dir_contents_only: true,
                    include_patterns: patterns,
                    exclude_patterns: cfg.exclude.clone(),
                    required_paths: required,
                    attempt_unpack: unpack,
                    create_dest_path: true,
                    allow_wildcard: true,
                    allow_empty_wildcard: true,
                    chown: chown.clone(),
                    created: None,
                },
            });
        }
        let ds_platform = self.states.get(d).and_then(|s| s.platform.clone());
        for c in &cfg.sources.contents {
            msg.extend_from_slice(&errb(&[b" <<", &c.path]));
            let mut st = State::scratch();
            st.platform = ds_platform.clone();
            let made = self.graph.file(
                &st,
                vec![Action::Mkfile {
                    path: c.path.clone(),
                    mode: 0o644,
                    data: c.data.clone(),
                    chown: None,
                    created: None,
                }],
                custom_name(b"[internal] preparing inline document".to_vec()),
            );
            actions.push(Action::Copy {
                from: made.output,
                from_dir: b"/".to_vec(),
                src: c.path.clone(),
                dest: dest.clone(),
                info: CopyInfo {
                    mode: mode.clone(),
                    create_dest_path: true,
                    chown: chown.clone(),
                    exclude_patterns: cfg.exclude.clone(),
                    ..CopyInfo::default()
                },
            });
        }
        msg.extend_from_slice(&errb(&[b" ", &cfg.sources.dest]));
        if let Some(onto) = &cfg.onto {
            let mut meta = custom_name(pg_name);
            meta.locations.push(cfg.location.clone());
            return Ok(Some(self.graph.file(onto, actions, meta)));
        }
        let msg = cfg.history.clone().unwrap_or(msg);
        let ds = self.ds(d)?;
        let state = ds.state.clone();
        if cfg.link && cfg.chmod.is_empty() {
            // --link: the files land on scratch, merged onto the stage after.
            let group = self.graph.progress_group();
            let ds = self.ds(d)?;
            ds.cmd_index -= 1;
            let pg_name = prefix_command(ds, &name, multi.as_ref(), Some(&platform), &env);
            let ignore_cache = ds.ignore_cache;
            let mut copy_meta = custom_name(pg_name.clone());
            copy_meta.ignore_cache = ignore_cache;
            copy_meta.locations.push(cfg.location.clone());
            copy_meta.progress_group = Some(ProgressGroup {
                id: group,
                name: pg_name.clone(),
                weak: true,
            });
            let ds = self.ds(d)?;
            ds.cmd_index -= 1;
            let link_name = prefix_command(
                ds,
                &errb(&[b"LINK ", &name]),
                multi.as_ref(),
                Some(&platform),
                &env,
            );
            let mut merge_meta = custom_name(link_name);
            merge_meta.ignore_cache = ignore_cache;
            merge_meta.locations.push(cfg.location.clone());
            merge_meta.progress_group = Some(ProgressGroup {
                id: group,
                name: pg_name,
                weak: false,
            });
            let mut scratch = State::scratch();
            scratch.platform = self.states.get(d).and_then(|s| s.platform.clone());
            let copied = self.graph.file(&scratch, actions, copy_meta);
            let out = self.graph.merge(&[state.output, copied.output], merge_meta);
            let ds = self.ds(d)?;
            ds.state.output = out;
        } else {
            let mut meta = custom_name(pg_name);
            meta.ignore_cache = ds.ignore_cache;
            meta.locations.push(cfg.location.clone());
            let next = self.graph.file(&state, actions, meta);
            let ds = self.ds(d)?;
            ds.state.output = next.output;
        }
        let ds = self.ds(d)?;
        commit(ds, msg, true, true);
        Ok(None)
    }

    /// dockerui's `NamedContext`, as Dockerfile2LLB asks it (namedContextFunc): none for
    /// `scratch` or `context`; else the context keyed by the name's familiar form (its
    /// `:latest` dropped) with the platform, `NAME::os/arch`, or without.
    fn named_context(&self, name: &[u8], platform: Option<&Platform>) -> Result<Option<Named>, Fail> {
        if self.opts.contexts.is_empty()
            || equal_fold_name(b"scratch", name)
            || equal_fold_name(b"context", name)
        {
            return Ok(None);
        }
        let r = parse_normalized(name)
            .map_err(|e| Fail::new(errb(&[b"invalid context name ", name, b": ", &e])))?;
        let familiar = r.familiar();
        let familiar = familiar
            .strip_suffix(":latest")
            .unwrap_or(&familiar)
            .as_bytes()
            .to_vec();
        let p = platform.cloned().unwrap_or_else(|| self.target_platform.clone());
        let keyed = errb(&[&familiar, b"::", &platform::format_all(&platform::normalize(&p))]);
        for key in [keyed, familiar.clone()] {
            if let Some(v) = self.opts.contexts.get(&key) {
                return Ok(Some(Named {
                    name: familiar,
                    key,
                    value: v.clone(),
                }));
            }
        }
        Ok(None)
    }

    /// `NamedContext.Load`: the state a named context is, for stage `d` on `platform`,
    /// and the image config it brings, if it is an image.
    fn load_named(
        &mut self,
        d: usize,
        n: &Named,
        platform: &Platform,
    ) -> Result<(State, Option<Image>), Fail> {
        let input = n.value.clone();
        let Some(colon) = input.iter().position(|&b| b == b':') else {
            return Err(Fail::new(errb(&[
                b"invalid context specifier ",
                &input,
                b" for ",
                &n.key,
            ])));
        };
        let (kind, rest) = (go::head(&input, colon), go::tail(&input, colon + 1));
        let kind: &[u8] = if kind.starts_with(b"git@") { b"git" } else { kind };
        let git = |planner: &mut Self| -> Result<Option<State>, Fail> {
            match git::parse_git_ref(&input) {
                git::Parsed::Git(g) => {
                    let name = errb(&[b"[internal] load git source ", &input]);
                    Ok(Some(planner.git_source(
                        &g,
                        g.keep_git_dir == Some(true),
                        &g.checksum,
                        &name,
                    )))
                }
                git::Parsed::BadGit(e) => Err(Fail::new(e)),
                git::Parsed::NotGit => Ok(None),
            }
        };
        match kind {
            b"docker-image" => {
                let r = rest.strip_prefix(b"//").unwrap_or(rest);
                if r == EMPTY_IMAGE {
                    return Ok((State::scratch(), None));
                }
                let mut named = parse_normalized(r).map_err(Fail::new)?;
                if named.tag.is_none() && named.digest.is_none() {
                    named.tag = Some("latest".into());
                }
                let log = errb(&[b"[context ", &n.key, b"] load metadata for ", r]);
                let resolved = self
                    .resolver
                    .resolve(named.to_string().as_bytes(), platform, &log)
                    .map_err(Fail::new)?;
                let mut reference = parse_normalized(&resolved.reference).map_err(Fail::new)?;
                if reference.tag.is_none() && reference.digest.is_none() {
                    reference.tag = Some("latest".into());
                }
                let mut img = Image::from_json(&resolved.config).map_err(Fail::new)?;
                img.created = None;
                let mut state = self.graph.source(
                    [b"docker-image://".as_slice(), reference.to_string().as_bytes()].concat(),
                    BTreeMap::new(),
                    Some(platform.clone()),
                    custom_name(errb(&[b"[context ", &n.key, b"] ", r])),
                );
                with_image_config(&mut state, &img);
                Ok((state, Some(img)))
            }
            b"git" => match git(self)? {
                Some(st) => Ok((st, None)),
                None => Err(Fail::new(errb(&[b"invalid git context ", &input]))),
            },
            b"http" | b"https" => {
                if let Some(st) = git(self)? {
                    return Ok((st, None));
                }
                let mut attrs = BTreeMap::new();
                attrs.insert(b"http.filename".to_vec(), b"context".to_vec());
                let st = self.graph.source(
                    input.clone(),
                    attrs,
                    None,
                    custom_name(errb(&[b"[context ", &n.key, b"] ", &input])),
                );
                Ok((st, None))
            }
            b"local" => {
                let st = self.graph.source(
                    [b"local://".as_slice(), rest].concat(),
                    BTreeMap::new(),
                    None,
                    custom_name(errb(&[b"[context ", &n.key, b"] load from client"])),
                );
                if let Some(out) = st.output {
                    self.named_locals.push((out, d, n.key.clone(), rest.to_vec()));
                }
                Ok((st, None))
            }
            // An image of an OCI layout the client serves as a content store: its store's
            // name and the manifest's digest, under a stand-in reference made of the
            // context's name and that digest.
            b"oci-layout" => {
                let spec = rest.strip_prefix(b"//").unwrap_or(rest);
                let q = |b: &[u8]| go::quote(b);
                let r = std::str::from_utf8(spec)
                    .map_err(|_| "invalid reference format".to_string())
                    .and_then(|s| Reference::parse_as_written(s).map_err(|e| e.to_string()))
                    .map_err(|e| {
                        Fail::new(errb(&[
                            b"could not parse oci-layout reference ",
                            q(spec).as_bytes(),
                            b": ",
                            e.as_bytes(),
                        ]))
                    })?;
                let Some(digest) = r.digest.clone() else {
                    return Err(Fail::new(errb(&[
                        b"oci-layout reference ",
                        q(r.to_string().as_bytes()).as_bytes(),
                        b" has no digest",
                    ])));
                };
                let mut dummy = parse_normalized(&n.name).map_err(|e| {
                    Fail::new(errb(&[
                        b"could not parse oci-layout reference ",
                        q(&n.name).as_bytes(),
                        b": ",
                        &e,
                    ]))
                })?;
                dummy.digest = Some(digest);
                let dummy = dummy.to_string().into_bytes();
                let log = errb(&[b"[context ", &n.key, b"] load metadata for ", &dummy]);
                let resolved = self.resolver.resolve(&dummy, platform, &log).map_err(Fail::new)?;
                let mut img = Image::from_json(&resolved.config)
                    .map_err(|e| Fail::new(errb(&[b"could not parse oci-layout image config: ", &e])))?;
                img.created = None;
                let mut attrs = BTreeMap::new();
                attrs.insert(b"oci.store".to_vec(), r.name().into_bytes());
                let mut state = self.graph.source(
                    [b"oci-layout://".as_slice(), &dummy].concat(),
                    attrs,
                    Some(platform.clone()),
                    custom_name(errb(&[b"[context ", &n.key, b"] OCI load from client"])),
                );
                with_image_config(&mut state, &img);
                Ok((state, Some(img)))
            }
            // buildx gives the frontend no inputs for a context it names.
            b"input" => Err(Fail::new(errb(&[b"invalid input ", rest, b" for ", &n.key]))),
            other => Err(Fail::new(errb(&[
                b"unsupported context source ",
                other,
                b" for ",
                &n.key,
            ]))),
        }
    }

    /// Whether the file declares an agent or harness of this name, in any stage.
    /// For `COPY --from=<agent>`, the stage that declares the agent: dispatched first, as a
    /// stage copied from is, so that its source is expanded in its own scope (D56).
    fn declaring_stage(&self, command: &Command) -> Option<usize> {
        let from = match &command.kind {
            Kind::Copy(c) => &c.from,
            Kind::Agentfile(crate::agentfile::Directive::Skill(sk)) => &sk.from,
            _ => return None,
        };
        if from.is_empty() {
            return None;
        }
        let name = go::to_lower(from);
        self.states.iter().position(|s| {
            s.stage.commands.iter().any(|c| match &c.kind {
                Kind::Agentfile(crate::agentfile::Directive::Agent(a))
                | Kind::Agentfile(crate::agentfile::Directive::Harness(a)) => a.name == name,
                _ => false,
            })
        })
    }

    fn declares_domain(&self, name: &[u8]) -> bool {
        let name = go::to_lower(name);
        self.states.iter().any(|s| {
            s.stage.commands.iter().any(|c| match &c.kind {
                Kind::Agentfile(crate::agentfile::Directive::Agent(a))
                | Kind::Agentfile(crate::agentfile::Directive::Harness(a)) => a.name == name,
                _ => false,
            })
        })
    }

    /// The kind of the domain `name` names in stage `d`'s lineage: `kind` if said, else
    /// what declares it (agentfile::check has refused a name both declare unsaid).
    fn domain_kind(
        &self,
        d: usize,
        name: &[u8],
        kind: Option<crate::agentfile::TargetKind>,
    ) -> crate::agentfile::TargetKind {
        use crate::agentfile::{Directive, TargetKind};
        if let Some(k) = kind {
            return k;
        }
        let mut at = Some(d);
        while let Some(i) = at {
            let Some(s) = self.states.get(i) else { break };
            for c in &s.stage.commands {
                match &c.kind {
                    Kind::Agentfile(Directive::Agent(a)) if a.name == name => return TargetKind::Agent,
                    Kind::Agentfile(Directive::Harness(h)) if h.name == name => return TargetKind::Harness,
                    _ => {}
                }
            }
            at = s.base;
        }
        TargetKind::Agent
    }

    /// Where a grant to `scope`'s names goes (§12.1): beside each grantee's directory,
    /// in `<dir>.d/<what>`: `/agents/<name>.d/<what>` for an agent where it unpacks by
    /// default.
    fn grant_dirs(
        &self,
        d: usize,
        scope: &crate::agentfile::Scope,
        what: &[u8],
    ) -> Result<Vec<Vec<u8>>, Fail> {
        scope
            .names
            .iter()
            .map(|n| {
                let harness = self.domain_kind(d, n, scope.kind) == crate::agentfile::TargetKind::Harness;
                let dir = self.declared_dir(d, n, harness)?;
                Ok(errb(&[&dir, b".d/", what]))
            })
            .collect()
    }

    /// The directory of the domain `name` of stage `d`'s lineage, as its directive lays it.
    fn declared_dir(&self, d: usize, name: &[u8], harness: bool) -> Result<Vec<u8>, Fail> {
        use crate::agentfile::Directive;
        let mut at = Some(d);
        while let Some(i) = at {
            let Some(s) = self.states.get(i) else { break };
            for c in &s.stage.commands {
                match &c.kind {
                    Kind::Agentfile(Directive::Agent(a)) if !harness && a.name == name => {
                        return domain_dir(a, false).map_err(Fail::new);
                    }
                    Kind::Agentfile(Directive::Harness(h)) if harness && h.name == name => {
                        return domain_dir(h, true).map_err(Fail::new);
                    }
                    _ => {}
                }
            }
            at = s.base;
        }
        Ok(errb(&[
            if harness {
                b"/harness/".as_slice()
            } else {
                b"/agents/"
            },
            name,
        ]))
    }

    /// An agent's, harness's or MCP server's source fetched into `dest` as a layer of its
    /// own (`ADD --link`, §7 Q19): a directory's files, a local archive or a URL's
    /// unpacked, a Git repository's tree.
    #[allow(clippy::too_many_arguments)]
    fn fetch_into(
        &mut self,
        d: usize,
        kind: &[u8],
        source: &[u8],
        dest: &[u8],
        code: &[u8],
        loc: &Location,
        lint: &LinterView<'_>,
    ) -> Result<(), Fail> {
        if let crate::agentfile::Source::Oci(r) = crate::agentfile::source_of(source).map_err(Fail::new)? {
            return self.artifact_into(d, kind, &r, dest, code, loc, lint);
        }
        let _ = lint;
        let content = self.content_of(d, source)?;
        let mut dest = dest.to_vec();
        if !dest.ends_with(b"/") {
            dest.push(b'/');
        }
        let dest_of_own = dest.clone();
        let cfg = CopyConfig {
            sources: instructions::Sources {
                dest,
                paths: vec![b"/".to_vec()],
                contents: Vec::new(),
            },
            exclude: Vec::new(),
            from: Some(content),
            is_add: false,
            code: code.to_vec(),
            chown: Vec::new(),
            chmod: Vec::new(),
            link: true,
            keep_git_dir: None,
            checksum: Vec::new(),
            parents: false,
            unpack: None,
            onto: None,
            history: Some(code.to_vec()),
            location: loc.clone(),
        };
        let first = self.graph.vertices.len();
        self.dispatch_copy(d, cfg, loc, lint)?;
        self.own(first, dest_of_own);
        Ok(())
    }

    /// What a path, Git or http(s) source holds, at the root of a state of its own: a
    /// Git repository's tree; a URL's download, or a path of the context, unpacked where it
    /// is an archive, as `ADD` takes them.
    fn content_of(&mut self, d: usize, source: &[u8]) -> Result<State, Fail> {
        let shown = errb(&[b"[internal] load ", source]);
        let mut scratch = State::scratch();
        scratch.platform = self.states.get(d).and_then(|s| s.platform.clone());
        let copy = |from: State, src: Vec<u8>, contents: bool| Action::Copy {
            from: from.output,
            from_dir: from.dir.clone(),
            src,
            dest: b"/".to_vec(),
            info: CopyInfo {
                follow_symlinks: true,
                dir_contents_only: contents,
                attempt_unpack: true,
                create_dest_path: true,
                allow_wildcard: contents,
                allow_empty_wildcard: contents,
                ..CopyInfo::default()
            },
        };
        match crate::agentfile::source_of(source).map_err(Fail::new)? {
            crate::agentfile::Source::Git(g) => {
                Ok(self.git_source(&g, g.keep_git_dir == Some(true), &g.checksum, &shown))
            }
            crate::agentfile::Source::Http(url) => {
                let name = http_filename(&url);
                let mut attrs = BTreeMap::new();
                attrs.insert(b"http.filename".to_vec(), name.clone());
                let st = self.graph.source(url, attrs, None, custom_name(shown.clone()));
                Ok(self
                    .graph
                    .file(&scratch, vec![copy(st, name, false)], custom_name(shown)))
            }
            crate::agentfile::Source::Path(p) => {
                let src = normalize_path(b"/", &p, false);
                if let Some(ds) = self.states.get_mut(d) {
                    ds.ctx_paths.insert(src.clone());
                }
                let mut context = State::scratch();
                context.output = Some(self.context);
                Ok(self
                    .graph
                    .file(&scratch, vec![copy(context, src, true)], custom_name(shown)))
            }
            crate::agentfile::Source::Oci(r) => Err(Fail::new(errb(&[
                r.as_slice(),
                b": an OSI artifact's content is taken by its own directive",
            ]))),
        }
    }

    /// The content of the agent or harness `name` (Q19.1): what its directive lays at its
    /// directory, at the root; its source as its own stage expanded it.
    fn domain_content(&mut self, d: usize, name: &[u8]) -> Result<State, Fail> {
        let name = go::to_lower(name);
        if let Some(st) = self.contents.get(&name) {
            return Ok(st.clone());
        }
        // Its stage, a dependency, was dispatched first.
        let Some((kind, source)) = self.domain_sources.get(&name).cloned() else {
            return Err(Fail::new(errb(&[
                b"COPY --from=",
                &name,
                b": the agent or harness is declared by a stage after this one",
            ])));
        };
        let kind = kind.as_slice();
        let st = match crate::agentfile::source_of(&source).map_err(Fail::new)? {
            crate::agentfile::Source::Oci(r) => {
                let log = errb(&[b"[internal] load metadata for ", &r]);
                let resolved = self.resolver.artifact(&r, kind, &log).map_err(Fail::new)?;
                self.domain_configs.insert(name.clone(), resolved.config.clone());
                let mut attrs = BTreeMap::new();
                attrs.insert(b"osi.kind".to_vec(), kind.to_vec());
                self.graph.source(
                    [b"osi-artifact://".as_slice(), &resolved.reference].concat(),
                    attrs,
                    None,
                    custom_name(errb(&[b"[internal] load ", kind, b" ", &r])),
                )
            }
            _ => self.content_of(d, &source)?,
        };
        self.contents.insert(name, st.clone());
        Ok(st)
    }

    /// Marks the vertices made since `first` as a domain's own directive's, writing
    /// `dest` (D55).
    fn own(&mut self, first: usize, dest: Vec<u8>) {
        for v in self.graph.vertices.iter_mut().skip(first) {
            v.meta.description.insert(OWN.to_vec(), dest.clone());
        }
    }

    /// An OSI artifact (§8 Q1, §12.17) laid into `dest` as a layer of its own: its content,
    /// resolved by digest and checked by the resolver, and its config beside it, at
    /// `<dest>.d/osi.json`, for the runtime to read how it runs.
    #[allow(clippy::too_many_arguments)]
    fn artifact_into(
        &mut self,
        d: usize,
        kind: &[u8],
        reference: &[u8],
        dest: &[u8],
        code: &[u8],
        loc: &Location,
        lint: &LinterView<'_>,
    ) -> Result<(), Fail> {
        let log = errb(&[b"[internal] load metadata for ", reference]);
        let resolved = self.resolver.artifact(reference, kind, &log).map_err(Fail::new)?;
        let mut attrs = BTreeMap::new();
        attrs.insert(b"osi.kind".to_vec(), kind.to_vec());
        let source = self.graph.source(
            [b"osi-artifact://".as_slice(), &resolved.reference].concat(),
            attrs,
            None,
            custom_name(errb(&[b"[internal] load ", kind, b" ", reference])),
        );
        let base = dest.strip_suffix(b"/").unwrap_or(dest).to_vec();
        let mut content_dest = base.clone();
        content_dest.push(b'/');
        let copy = |from: Option<State>,
                    paths: Vec<Vec<u8>>,
                    contents: Vec<instructions::SourceContent>,
                    dest: Vec<u8>| CopyConfig {
            sources: instructions::Sources {
                dest,
                paths,
                contents,
            },
            exclude: Vec::new(),
            from,
            is_add: false,
            code: code.to_vec(),
            chown: Vec::new(),
            chmod: Vec::new(),
            link: true,
            keep_git_dir: None,
            checksum: Vec::new(),
            parents: false,
            unpack: None,
            onto: None,
            history: Some(code.to_vec()),
            location: loc.clone(),
        };
        let first = self.graph.vertices.len();
        self.dispatch_copy(
            d,
            copy(Some(source), vec![b"/".to_vec()], Vec::new(), content_dest),
            loc,
            lint,
        )?;
        self.own(first, base.clone());
        let config = instructions::SourceContent {
            path: b"osi.json".to_vec(),
            data: resolved.config,
            expand: false,
        };
        let first = self.graph.vertices.len();
        self.dispatch_copy(
            d,
            copy(None, Vec::new(), vec![config], errb(&[&base, b".d/"])),
            loc,
            lint,
        )?;
        self.own(first, base.clone());
        Ok(())
    }

    /// `SKILL` (§4.3, §8 Q12): its source taken as `ADD` takes one, onto nothing; each
    /// skill it holds checked and laid out in a directory of its name (D54); then laid as
    /// a layer of its own where it goes: the given destination, `/skills/` for every
    /// agent, or each grantee's `.d/skills/`.
    /// What `SKILL --from=<agent> <skill>` takes (§12 answer 12): the agent's content, at
    /// whose root the skill is, once its OSI config is seen to list it. That is a
    /// declaration of what the agent brings, not one agent reading another: a path its
    /// config does not list, or an agent with no config (from a path, Git or http(s)), is
    /// refused.
    fn domain_skill(&mut self, d: usize, sk: &crate::agentfile::Skill) -> Result<State, Fail> {
        let content = self.domain_content(d, &sk.from)?;
        let name = go::to_lower(&sk.from);
        let crate::agentfile::SkillSource::Path(p) = &sk.source else {
            return Err(Fail::new(errb(&[
                b"SKILL --from=",
                &sk.from,
                b" takes a path its config lists, not a heredoc",
            ])));
        };
        let Some(config) = self.domain_configs.get(&name) else {
            return Err(Fail::new(errb(&[
                b"SKILL --from=",
                &sk.from,
                b": it has no OSI config listing its skills; only an agent or harness from an OSI artifact has one",
            ])));
        };
        let norm = |p: &[u8]| -> Vec<u8> {
            let p = p.strip_prefix(b"./").unwrap_or(p);
            p.strip_suffix(b"/").unwrap_or(p).to_vec()
        };
        let listed: Vec<Vec<u8>> = serde_json::from_slice::<serde_json::Value>(config)
            .ok()
            .and_then(|c| c.get("skills").and_then(|s| s.as_array()).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.as_str().map(|s| norm(s.as_bytes())))
            .collect();
        if !listed.contains(&norm(p)) {
            return Err(Fail::new(errb(&[
                b"SKILL --from=",
                &sk.from,
                b" ",
                p,
                b": its config lists no such skill (it lists ",
                &listed.join(&b", "[..]),
                b")",
            ])));
        }
        Ok(content)
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_skill(
        &mut self,
        d: usize,
        sk: &crate::agentfile::Skill,
        sources: &[usize],
        domain: Option<State>,
        code: &[u8],
        loc: &Location,
        lint: &LinterView<'_>,
    ) -> Result<(), Fail> {
        if sk.dest.is_some() && !sk.scope.names.is_empty() {
            return Err(Fail::new(
                b"SKILL ... FOR lays the skill in each grantee's own skills directory: drop the destination"
                    .to_vec(),
            ));
        }
        let (paths, contents) = match &sk.source {
            crate::agentfile::SkillSource::Path(p) => (vec![p.clone()], Vec::new()),
            crate::agentfile::SkillSource::Text(t) => (Vec::new(), vec![t.clone()]),
        };
        // The directory a source that is one skill comes as, for the check of its name.
        let base = |p: &[u8]| -> Vec<u8> {
            let trimmed = p.strip_suffix(b"/").unwrap_or(p);
            trimmed.rsplit(|&b| b == b'/').next().unwrap_or_default().to_vec()
        };
        let name = match paths.first() {
            None => Vec::new(),
            Some(p) => match git::parse_git_ref(p) {
                git::Parsed::Git(g) if !g.indistinguishable_from_local => {
                    if g.subdir.is_empty() {
                        g.short_name.clone()
                    } else {
                        base(&g.subdir)
                    }
                }
                // What a URL serves is named by no directory.
                _ if is_http_source(p) => Vec::new(),
                _ => base(p),
            },
        };
        let taken = domain.is_some();
        let from = match (domain, sources.first()) {
            (Some(content), _) => Some(content),
            (None, Some(&s)) => Some(self.ds(s)?.state.clone()),
            (None, None) => None,
        };
        let mut scratch = State::scratch();
        scratch.platform = self.states.get(d).and_then(|s| s.platform.clone());
        if sources.is_empty()
            && !taken
            && let crate::agentfile::SkillSource::Path(p) = &sk.source
            && !is_http_source(p)
            && !matches!(git::parse_git_ref(p), git::Parsed::Git(g) if !g.indistinguishable_from_local)
        {
            self.ds(d)?.ctx_paths.insert(go::join(&[b"/", p]));
        }
        let fetched = self
            .dispatch_copy(
                d,
                CopyConfig {
                    sources: instructions::Sources {
                        dest: b"/".to_vec(),
                        paths,
                        contents,
                    },
                    exclude: sk.exclude.clone(),
                    from,
                    is_add: true,
                    code: code.to_vec(),
                    chown: Vec::new(),
                    chmod: Vec::new(),
                    link: false,
                    keep_git_dir: sk.keep_git_dir,
                    checksum: sk.checksum.clone(),
                    parents: false,
                    unpack: None,
                    onto: Some(scratch),
                    history: None,
                    location: loc.clone(),
                },
                loc,
                lint,
            )?
            .ok_or_else(|| Fail::new(b"SKILL fetched nothing".to_vec()))?;
        let multi = self.named_by_platform();
        let ds = self.ds(d)?;
        let platform = ds.platform.clone();
        let env = ds.state.env.clone();
        let check = prefix_command(
            ds,
            &errb(&[b"SKILL checking ", &name]),
            multi.as_ref(),
            platform.as_ref(),
            &env,
        );
        let laid = self.graph.skills(&fetched, &name, custom_name(check));
        let dests = match (&sk.dest, sk.scope.names.is_empty()) {
            (Some(dest), _) => vec![dest.clone()],
            (None, true) => vec![b"/skills/".to_vec()],
            (None, false) => self.grant_dirs(d, &sk.scope, b"skills")?,
        };
        for dest in dests {
            let mut dest = dest;
            if !dest.ends_with(b"/") {
                dest.push(b'/');
            }
            let cfg = CopyConfig {
                sources: instructions::Sources {
                    dest,
                    paths: vec![b"/".to_vec()],
                    contents: Vec::new(),
                },
                exclude: Vec::new(),
                from: Some(laid.clone()),
                is_add: false,
                code: code.to_vec(),
                chown: sk.chown.clone(),
                chmod: sk.chmod.clone(),
                link: true,
                keep_git_dir: None,
                checksum: Vec::new(),
                parents: false,
                unpack: None,
                onto: None,
                history: Some(code.to_vec()),
                location: loc.clone(),
            };
            let first = self.graph.vertices.len();
            let mark = cfg.sources.dest.clone();
            self.dispatch_copy(d, cfg, loc, lint)?;
            self.own(first, mark);
        }
        Ok(())
    }

    /// `llb.Git`: the repository as a source, its ID and attributes as BuildKit makes
    /// them ([`git_identifier`]).
    fn git_source(&mut self, g: &git::GitRef, keep_git_dir: bool, checksum: &[u8], name: &[u8]) -> State {
        let (identifier, attrs) = git_identifier(g, keep_git_dir, checksum);
        self.graph
            .source(identifier, attrs, None, custom_name(name.to_vec()))
    }

    fn finalize(mut self, target: usize) -> Result<Plan, Fail> {
        // Every file operation and command of the target's lineage may write in no
        // domain but its own (D55).
        if self.states.get(target).is_some_and(|t| !t.domains.is_empty()) {
            let mut lineage = Vec::new();
            let mut at = Some(target);
            while let Some(i) = at {
                lineage.push(i);
                at = self.states.get(i).and_then(|s| s.base);
            }
            for (stage, range) in std::mem::take(&mut self.made) {
                if !lineage.contains(&stage) {
                    continue;
                }
                for v in self.graph.vertices.get_mut(range).unwrap_or_default() {
                    if matches!(v.kind, llb::Kind::Exec { .. } | llb::Kind::File { .. }) {
                        v.meta.description.insert(GUARD.to_vec(), b"1".to_vec());
                    }
                }
            }
        }
        let ctx_paths: BTreeSet<Vec<u8>> = self
            .states
            .iter()
            .flat_map(|s| s.ctx_paths.iter().cloned())
            .collect();
        let labels = self.opts.labels.clone();
        let t = self.ds(target)?;
        t.image.config.labels.extend(labels);
        if self.lint.failed() {
            return Err(Fail::new(self.lint.error_message()));
        }
        // The build context, with only the paths the stages copy from it.
        let mut attrs = BTreeMap::new();
        if !self.opts.excludes.is_empty() {
            let mut json = String::new();
            crate::json::write_strings(&mut json, &self.opts.excludes);
            attrs.insert(b"local.excludepatterns".to_vec(), json.into_bytes());
        }
        if let Some(paths) = normalize_context_paths(&ctx_paths) {
            let mut json = String::new();
            crate::json::write_strings(&mut json, &paths);
            attrs.insert(b"local.followpaths".to_vec(), json.into_bytes());
        }
        attrs.insert(b"local.sharedkeyhint".to_vec(), b"context".to_vec());
        attrs.insert(b"local.unique".to_vec(), self.opts.context_id.clone());
        // Each local named context, with only the paths its stage copies from it
        // (asyncLocalOutput, as it is marshalled once every stage is dispatched).
        for (out, d, key, name) in std::mem::take(&mut self.named_locals) {
            let mut a = BTreeMap::new();
            let paths = self
                .states
                .get(d)
                .and_then(|s| self.path_sets.get(s.paths))
                .cloned()
                .unwrap_or_default();
            if let Some(paths) = normalize_context_paths(&paths) {
                let mut json = String::new();
                crate::json::write_strings(&mut json, &paths);
                a.insert(b"local.followpaths".to_vec(), json.into_bytes());
            }
            if let Some(ex) = self.opts.context_excludes.get(&name).filter(|e| !e.is_empty()) {
                let mut json = String::new();
                crate::json::write_strings(&mut json, ex);
                a.insert(b"local.excludepatterns".to_vec(), json.into_bytes());
            }
            let shared = self.opts.context_keys.get(&key).cloned().unwrap_or_default();
            a.insert(
                b"local.sharedkeyhint".to_vec(),
                errb(&[b"context:", &key, b"-", &shared]),
            );
            a.insert(b"local.unique".to_vec(), self.opts.context_id.clone());
            if let Some(v) = self.graph.vertices.get_mut(out.vertex)
                && let llb::Kind::Source { attrs: at, .. } = &mut v.kind
            {
                *at = a;
            }
        }
        if let Some(v) = self.graph.vertices.get_mut(self.context.vertex) {
            if let llb::Kind::Source { attrs: a, .. } = &mut v.kind {
                *a = attrs;
            }
            v.meta = custom_name(b"[internal] load build context".to_vec());
        }
        // The SBOM scanner's other targets: each stage's own `ARG` overriding the global one.
        let scans = |ds: &Ds, name: &[u8]| -> bool {
            let mut on = self
                .global_args
                .get(name)
                .is_some_and(|v| enabled_for_stage(&ds.stage_name, v));
            for a in &ds.build_args {
                if a.key == name
                    && let Some(v) = &a.value
                {
                    on = enabled_for_stage(&ds.stage_name, v);
                }
            }
            on
        };
        let mut sbom_extras: BTreeMap<Vec<u8>, State> = BTreeMap::new();
        if let Some(t) = self.states.get(target)
            && scans(t, SBOM_SCAN_CONTEXT)
        {
            sbom_extras.insert(
                b"context".to_vec(),
                State {
                    output: Some(self.context),
                    ..State::scratch()
                },
            );
        }
        for i in self.reachable(target) {
            if let Some(ds) = self.states.get(i)
                && i != target
                && scans(ds, SBOM_SCAN_STAGE)
            {
                sbom_extras.insert(ds.stage_name.clone(), ds.state.clone());
            }
        }
        let platform = self.target_platform.clone();
        let t = self
            .states
            .get_mut(target)
            .ok_or_else(|| Fail::new(b"no target".to_vec()))?;
        let mut image = std::mem::take(&mut t.image);
        // An explicit target platform is the image's.
        let same = platform.os == image.platform.os && platform.architecture == image.platform.architecture;
        image.platform.os = platform.os.clone();
        image.platform.architecture = platform.architecture.clone();
        if !platform.variant.is_empty() || !same {
            image.platform.variant = platform.variant.clone();
        }
        if !platform.os_version.is_empty() || !same {
            image.platform.os_version = platform.os_version.clone();
        }
        if !platform.os_features.is_empty() {
            image.platform.os_features = platform.os_features.clone();
        }
        image.platform = platform::normalize(&image.platform);
        // An Agentfile's directives travel in a layer of their own, the normalized
        // Agentfile, and its digest in a label (§8, D35).
        if !t.agentfile.is_empty() {
            crate::agentfile::reach(&t.agentfile).map_err(Fail::new)?;
            crate::agentfile::ingress(&t.agentfile).map_err(Fail::new)?;
            crate::agentfile::connections(&t.agentfile).map_err(Fail::new)?;
            if crate::agentfile::dns(&t.agentfile) {
                image
                    .config
                    .labels
                    .insert(crate::agentfile::DNS_LABEL.to_vec(), b"1".to_vec());
            }
            let servers = crate::agentfile::remote_mcp(&t.agentfile);
            if !servers.is_empty() {
                image
                    .config
                    .labels
                    .insert(crate::agentfile::MCP_LABEL.to_vec(), servers.join(&b","[..]));
            }
            let declared = crate::agentfile::egress_declared(&t.agentfile);
            if !declared.is_empty() {
                image.config.labels.insert(
                    crate::agentfile::EGRESS_DECLARED_LABEL.to_vec(),
                    declared.join(&b","[..]),
                );
            }
            let egress = crate::agentfile::egress(&t.agentfile);
            if !egress.is_empty() {
                image
                    .config
                    .labels
                    .insert(crate::agentfile::EGRESS_LABEL.to_vec(), egress.join(&b","[..]));
            }
            let spec = crate::agentfile::spec(&t.agentfile);
            image.config.labels.insert(
                crate::agentfile::DIGEST_LABEL.to_vec(),
                crate::agentfile::digest(&spec),
            );
            let created = t.epoch.map(|(s, _)| s);
            let made = self.graph.file(
                &t.state,
                vec![Action::Mkfile {
                    path: crate::agentfile::SPEC_PATH.to_vec(),
                    mode: 0o444,
                    data: spec,
                    chown: None,
                    created,
                }],
                custom_name(b"[agentfile] the normalized Agentfile".to_vec()),
            );
            t.state = made;
            image.history.push(History {
                created: ds_epoch(t),
                created_by: errb(&[b"AGENTFILE ", crate::agentfile::SPEC_PATH]),
                author: Vec::new(),
                comment: HISTORY_COMMENT.to_vec(),
                empty_layer: false,
            });
        }
        Ok(Plan {
            state: std::mem::take(&mut t.state),
            graph: self.graph,
            image,
            platform,
            warnings: Vec::new(),
            epoch: self.epoch.map(|(s, _)| s),
            domains: self
                .states
                .get(target)
                .map(|t| t.domains.clone())
                .unwrap_or_default(),
            sbom_extras: sbom_extras.into_iter().collect(),
        })
    }
}

/// `isHTTPSource`: an http(s) URL that is no git repository.
fn is_http_source(src: &[u8]) -> bool {
    (src.starts_with(b"http://") || src.starts_with(b"https://"))
        && !matches!(git::parse_git_ref(src), git::Parsed::Git(_))
}

/// What `dispatchCopy` copies.
struct CopyConfig {
    sources: instructions::Sources,
    exclude: Vec<Vec<u8>>,
    /// The stage copied from; the build context when none.
    from: Option<State>,
    is_add: bool,
    code: Vec<u8>,
    chown: Vec<u8>,
    chmod: Vec<u8>,
    link: bool,
    /// `ADD --keep-git-dir`.
    keep_git_dir: Option<bool>,
    checksum: Vec<u8>,
    parents: bool,
    unpack: Option<bool>,
    /// Where the files land instead of the stage: a state the copy makes, returned and
    /// not committed (an Agentfile directive's own steps, D54).
    onto: Option<State>,
    /// The history's text, where it is not `ADD`'s or `COPY`'s own.
    history: Option<Vec<u8>>,
    /// The instruction's lines, the copy's location (D80).
    location: Location,
}

/// `validateCopySourcePath`: a warning for a source the .dockerignore excludes. Nothing is
/// said where its patterns hold an exclusion, which a file under an excluded directory may
/// be named by, nor of the context's root unless a pattern excludes every entry of it.
fn validate_copy_source_path(
    ignore: &mut crate::glob::PatternMatcher,
    src: &[u8],
    is_add: bool,
    loc: &Location,
    lint: &LinterView<'_>,
) {
    if ignore.exclusions() {
        return;
    }
    let src = go::clean(src);
    let root_ignored = || {
        ignore
            .patterns()
            .iter()
            .any(|p| !p.exclusion() && matches!(p.text(), b"*" | b"**" | b"**/*"))
    };
    if (src == b"." || src == b"/") && !root_ignored() {
        return;
    }
    // Its error is ignored, as dispatchCopy ignores it.
    if ignore.matches_or_parent_matches(&src).unwrap_or(false) {
        let cmd: &[u8] = if is_add { b"Add" } else { b"Copy" };
        let msg = errb(&[
            b"Attempting to ",
            cmd,
            b" file ",
            go::quote(&src).as_bytes(),
            b" that is excluded by .dockerignore",
        ]);
        lint.run(&lint::COPY_IGNORED_FILE, loc, Some(&msg));
    }
}

fn set_dep(deps: &mut Vec<(usize, Location)>, s: usize, loc: &Location) {
    match deps.iter_mut().find(|(d, _)| *d == s) {
        Some(e) => e.1 = loc.clone(),
        None => deps.push((s, loc.clone())),
    }
}

/// `strings.Index`: where `needle` first is in `hay`; an empty one is at 0.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `filepath.Join` on Unix.
fn filepath_join(a: &[u8], b: &[u8]) -> Vec<u8> {
    go::join(&[a, b])
}

fn path_base(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
    let mut end = p.len();
    while end > 0 && p.get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    let t = go::head(p, end);
    if t.is_empty() {
        return b"/".to_vec();
    }
    match t.iter().rposition(|&b| b == b'/') {
        Some(at) => go::tail(t, at + 1).to_vec(),
        None => t.to_vec(),
    }
}

fn contains_wildcards(name: &[u8]) -> bool {
    let mut i = 0;
    while let Some(&c) = name.get(i) {
        match c {
            b'*' | b'?' | b'[' => return true,
            b'\\' => i += 1,
            _ => {}
        }
        i += 1;
    }
    false
}

/// `system.NormalizePath` for Linux.
fn normalize_path(parent: &[u8], new: &[u8], keep_slash: bool) -> Vec<u8> {
    let mut parent = if parent.is_empty() {
        b"/".to_vec()
    } else {
        parent.to_vec()
    };
    if !go::is_abs(&parent) {
        parent = go::join(&[b"/", &parent]);
    }
    let orig = new;
    let mut p = if new.is_empty() {
        parent.clone()
    } else {
        new.to_vec()
    };
    if !go::is_abs(&p) {
        p = go::join(&[&parent, &p]);
    }
    if keep_slash {
        if orig.ends_with(b"/") && !p.ends_with(b"/") {
            p.push(b'/');
        } else if orig.ends_with(b"/.") {
            if p != b"/" {
                p.push(b'/');
            }
            p.push(b'.');
        }
    }
    p
}

/// `pathRelativeToWorkingDir`.
fn path_relative_to_working_dir(dir: &[u8], p: &[u8]) -> Vec<u8> {
    if go::is_abs(p) {
        return normalize_path(b"/", p, true);
    }
    let p: &[u8] = if p == b"." || p.is_empty() { b"./" } else { p };
    normalize_path(dir, p, true)
}

/// `parseKeyValue`.
fn parse_key_value(env: &[u8]) -> (&[u8], &[u8]) {
    match env.iter().position(|&b| b == b'=') {
        Some(at) => (go::head(env, at), go::tail(env, at + 1)),
        None => (env, b""),
    }
}

/// `addEnv`: replaces the key's entry in place, or appends one.
fn add_env(env: &mut Vec<Vec<u8>>, k: &[u8], v: &[u8]) {
    let entry = errb(&[k, b"=", v]);
    match env.iter_mut().find(|e| parse_key_value(e).0 == k) {
        Some(e) => *e = entry,
        None => env.push(entry),
    }
}

/// `commitToHistory`.
fn commit(ds: &mut Ds, mut msg: Vec<u8>, with_layer: bool, with_state: bool) {
    if with_state {
        msg.extend_from_slice(b" # buildkit");
    }
    ds.image.history.push(History {
        created: ds_epoch(ds),
        created_by: msg,
        author: Vec::new(),
        comment: HISTORY_COMMENT.to_vec(),
        empty_layer: !with_layer,
    });
}

/// The time history entries carry: SOURCE_DATE_EPOCH's, in UTC.
fn ds_epoch(ds: &Ds) -> Option<go::Time> {
    ds.epoch.map(|(s, nanosecond)| go::Time {
        nanosecond,
        ..go::Time::from_unix(s)
    })
}

fn custom_name(name: Vec<u8>) -> Meta {
    let mut m = Meta::default();
    m.description.insert(b"llb.customname".to_vec(), name);
    m
}

/// `prefixCommand`: `[stage n/total] ` before a step's name, counting the step.
/// `prefixCommand`. `multi` is, for a build whose steps are named by platform, the
/// platform its frontend runs on, which completes a platform the environment names in part
/// (containerd's platforms.Parse): the build's own here.
fn prefix_command(
    ds: &mut Ds,
    s: &[u8],
    multi: Option<&Platform>,
    platform: Option<&Platform>,
    env: &EnvList,
) -> Vec<u8> {
    if ds.cmd_total == 0 {
        return s.to_vec();
    }
    let mut out = b"[".to_vec();
    if let (Some(host), Some(p)) = (multi, platform) {
        out.extend_from_slice(&platform::format_all(p));
        out.extend_from_slice(&format_target_platform(p, platform_from_env(env, host)));
        out.push(b' ');
    }
    if !ds.stage_name.is_empty() {
        out.extend_from_slice(&ds.stage_name);
        out.push(b' ');
    }
    ds.cmd_index += 1;
    // `%*d`: the index padded to the width of the total.
    let width = ds.cmd_total.to_string().len();
    out.extend_from_slice(format!("{:>width$}/{}] ", ds.cmd_index, ds.cmd_total).as_bytes());
    if ds.cmd_is_on_build {
        out.extend_from_slice(b"ONBUILD ");
    }
    out.extend_from_slice(s);
    out
}

fn format_target_platform(base: &Platform, target: Option<Platform>) -> Vec<u8> {
    let Some(mut t) = target else {
        return Vec::new();
    };
    if t.os.is_empty() {
        t.os = base.os.clone();
    }
    if t.architecture.is_empty() {
        t.architecture = base.architecture.clone();
    }
    let p = platform::normalize(&t);
    if p.os == base.os && p.architecture != base.architecture {
        let mut a = p.architecture.clone();
        if !p.variant.is_empty() {
            a.push(b'/');
            a.extend_from_slice(&p.variant);
        }
        return errb(&[b"->", &a]);
    }
    if p.os != base.os {
        return errb(&[b"->", &platform::format_all(&p)]);
    }
    Vec::new()
}

fn platform_from_env(env: &EnvList, host: &Platform) -> Option<Platform> {
    let mut p = Platform::default();
    let mut set = false;
    for k in env.keys() {
        let v = env.get(k).unwrap_or_default();
        match k {
            b"TARGETPLATFORM" => {
                if let Ok(p) = platform::parse(v, host) {
                    return Some(p);
                }
            }
            b"TARGETOS" => {
                p.os = v.to_vec();
                set = true;
            }
            b"TARGETARCH" => {
                p.architecture = v.to_vec();
                set = true;
            }
            b"TARGETVARIANT" => {
                p.variant = v.to_vec();
                set = true;
            }
            _ => {}
        }
    }
    set.then_some(p)
}

/// `uppercaseCmd`: the instruction's word uppercased.
fn uppercase_cmd(s: &[u8]) -> Vec<u8> {
    match s.iter().position(|&b| b == b' ') {
        Some(at) => errb(&[&go::to_upper(go::head(s, at)), go::tail(s, at)]),
        None => go::to_upper(s),
    }
}

/// `processCmdEnv`: the step as shown, variables expanded if they can be.
fn process_cmd_env(lexer: &Lex, cmd: &[u8], env: &EnvList) -> Vec<u8> {
    lexer
        .process(cmd, env)
        .map(|p| p.word)
        .unwrap_or_else(|_| cmd.to_vec())
}

fn with_shell(img: &Image, args: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut shell = if img.config.shell.is_empty() {
        if img.platform.os == b"windows" {
            vec![b"cmd".to_vec(), b"/S".to_vec(), b"/C".to_vec()]
        } else {
            vec![b"/bin/sh".to_vec(), b"-c".to_vec()]
        }
    } else {
        img.config.shell.clone()
    };
    shell.push(args.join(&b' '));
    shell
}

/// `runCommandString`: the build arguments in use, then the command.
fn run_command_string(args: &[Vec<u8>], build_args: &[ArgDef], env: &EnvList) -> Vec<u8> {
    let mut tmp: Vec<Vec<u8>> = Vec::new();
    let mut idx: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for a in build_args {
        let v = env
            .get(&a.key)
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| a.value.clone().unwrap_or_default());
        let e = errb(&[&a.key, b"=", &v]);
        match idx.get(&a.key) {
            Some(&i) => {
                if let Some(slot) = tmp.get_mut(i) {
                    *slot = e;
                }
            }
            None => {
                idx.insert(a.key.clone(), tmp.len());
                tmp.push(e);
            }
        }
    }
    if !tmp.is_empty() {
        tmp.insert(0, format!("|{}", tmp.len()).into_bytes());
    }
    tmp.extend(args.iter().cloned());
    tmp.join(&b' ')
}

/// Go's `%v` of a `[]string`: `[a b c]`.
fn go_list(v: &[Vec<u8>]) -> Vec<u8> {
    errb(&[b"[", &v.join(&b' '), b"]"])
}

/// Go's `%q` of a `[]string`: `["a" "b"]`.
fn quote_list(v: &[Vec<u8>]) -> Vec<u8> {
    let q: Vec<Vec<u8>> = v.iter().map(|s| go::quote(s).into_bytes()).collect();
    errb(&[b"[", &q.join(&b' '), b"]"])
}

/// Go's `%+v` of a `HealthcheckConfig`.
fn healthcheck_string(h: &crate::image::Healthcheck) -> Vec<u8> {
    errb(&[
        b"{Test:",
        &go_list(&h.test),
        b" Interval:",
        &go::format_duration(h.interval),
        b" Timeout:",
        &go::format_duration(h.timeout),
        b" StartPeriod:",
        &go::format_duration(h.start_period),
        b" StartInterval:",
        &go::format_duration(h.start_interval),
        format!(" Retries:{}}}", h.retries).as_bytes(),
    ])
}

fn legacy_kv(name: &[u8]) -> Vec<u8> {
    errb(&[
        b"\"",
        name,
        b" key=value\" should be used instead of legacy \"",
        name,
        b" key value\" format",
    ])
}

fn json_args(name: &[u8]) -> Vec<u8> {
    errb(&[
        b"JSON arguments recommended for ",
        name,
        b" to prevent unintended behavior related to OS signals",
    ])
}

fn validate_used_once(t: &mut Tracker, name: &[u8], loc: &Location, lint: &LinterView<'_>) {
    if let Some(prev) = &t.0 {
        let msg = errb(&[
            b"Multiple ",
            name,
            b" instructions should not be used in the same stage because only the last one will be used",
        ]);
        lint.run(&lint::MULTIPLE_INSTRUCTIONS_DISALLOWED, prev, Some(&msg));
    }
    t.0 = Some(loc.clone());
}

fn report_const_platform_disallowed(
    stage: &[u8],
    m: &lex::Processed,
    loc: &Location,
    lint: &LinterView<'_>,
    build: &Platform,
) {
    if !m.matched.is_empty() || !m.unmatched.is_empty() {
        return;
    }
    let Ok(p) = platform::parse(&m.word, build) else {
        return;
    };
    if find(stage, &p.os).is_some() || find(stage, &p.architecture).is_some() {
        return;
    }
    let msg = errb(&[
        b"FROM --platform flag should not use constant value ",
        go::quote(&m.word).as_bytes(),
    ]);
    lint.run(&lint::FROM_PLATFORM_FLAG_CONST_DISALLOWED, loc, Some(&msg));
}

/// `signal.ParseSignal` with Linux's signals.
fn parse_signal(s: &[u8]) -> Result<i64, Fail> {
    let invalid = || Fail::new(errb(&[b"invalid signal: ", s]));
    if let Some(n) = go_atoi(s) {
        return if n == 0 { Err(invalid()) } else { Ok(n) };
    }
    let upper = go::to_upper(s);
    let name = upper.strip_prefix(b"SIG").unwrap_or(&upper);
    const SIGNALS: &[(&[u8], i64)] = &[
        (b"ABRT", 6),
        (b"ALRM", 14),
        (b"BUS", 7),
        (b"CHLD", 17),
        (b"CLD", 17),
        (b"CONT", 18),
        (b"FPE", 8),
        (b"HUP", 1),
        (b"ILL", 4),
        (b"INT", 2),
        (b"IO", 29),
        (b"IOT", 6),
        (b"KILL", 9),
        (b"PIPE", 13),
        (b"POLL", 29),
        (b"PROF", 27),
        (b"PWR", 30),
        (b"QUIT", 3),
        (b"SEGV", 11),
        (b"STKFLT", 16),
        (b"STOP", 19),
        (b"SYS", 31),
        (b"TERM", 15),
        (b"TRAP", 5),
        (b"TSTP", 20),
        (b"TTIN", 21),
        (b"TTOU", 22),
        (b"URG", 23),
        (b"USR1", 10),
        (b"USR2", 12),
        (b"VTALRM", 26),
        (b"WINCH", 28),
        (b"XCPU", 24),
        (b"XFSZ", 25),
    ];
    if let Some((_, n)) = SIGNALS.iter().find(|(k, _)| *k == name) {
        return Ok(*n);
    }
    // RTMIN, RTMIN+1..15, RTMAX-14..1, RTMAX: 34 to 64.
    if name == b"RTMIN" {
        return Ok(34);
    }
    if name == b"RTMAX" {
        return Ok(64);
    }
    let rt = |prefix: &[u8], sign: i64, base: i64, max: i64| {
        name.strip_prefix(prefix)
            .and_then(|n| std::str::from_utf8(n).ok())
            .filter(|n| !n.starts_with('0') && n.bytes().all(|c| c.is_ascii_digit()))
            .and_then(|n| n.parse::<i64>().ok())
            .filter(|n| (1..=max).contains(n))
            .map(|n| base + sign * n)
    };
    rt(b"RTMIN+", 1, 34, 15)
        .or_else(|| rt(b"RTMAX-", -1, 64, 14))
        .ok_or_else(invalid)
}

/// `strconv.Atoi`: an optional sign and decimal digits, within `int64`.
fn go_atoi(s: &[u8]) -> Option<i64> {
    let digits = s.strip_prefix(b"+").or_else(|| s.strip_prefix(b"-")).unwrap_or(s);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse::<i64>().ok()
}

/// `ParseChmod` of a COPY or ADD: octal up to 07777, or a symbolic mode.
fn parse_chmod(chmod: &[u8]) -> Result<Option<Chmod>, Fail> {
    if chmod.is_empty() {
        return Ok(None);
    }
    let non_octal = || {
        Fail::new(errb(&[
            b"invalid chmod parameter: '",
            chmod,
            b"'. it should be octal string and between 0 and 07777",
        ]))
    };
    // strconv.ParseUint(chmod, 8, 32).
    if chmod.iter().all(|c| (b'0'..=b'7').contains(c)) {
        let mut n: u64 = 0;
        for &c in chmod {
            n = n
                .checked_mul(8)
                .and_then(|n| n.checked_add(u64::from(c - b'0')))
                .ok_or_else(non_octal)?;
            if n > u64::from(u32::MAX) {
                return Err(non_octal());
            }
        }
        if n > 0o7777 {
            return Err(non_octal());
        }
        return Ok(Some(Chmod::Mode(n as u32)));
    }
    // A mode that starts with a digit is a number, octal or not one.
    if chmod.first().is_some_and(u8::is_ascii_digit) {
        return Err(non_octal());
    }
    if !symbolic_mode_ok(chmod) {
        return Err(Fail::new(b"invalid syntax".to_vec()));
    }
    Ok(Some(Chmod::Str(chmod.to_vec())))
}

/// Whether dchapes-mode's `Parse` takes `s` as a symbolic mode: clauses separated by
/// commas, each its who (`ugoa`), then operators (`+-=`) each with its permissions
/// (`rwxXst`, or `ugo` to copy).
fn symbolic_mode_ok(s: &[u8]) -> bool {
    let mut i = 0;
    loop {
        while s.get(i).is_some_and(|c| b"augo".contains(c)) {
            i += 1;
        }
        if i >= s.len() {
            return false;
        }
        loop {
            let Some(&op) = s.get(i) else {
                return false;
            };
            i += 1;
            if !b"+-=".contains(&op) {
                return false;
            }
            while s.get(i).is_some_and(|c| b"rstwXxugo".contains(c)) {
                i += 1;
            }
            match s.get(i) {
                None => return true,
                Some(b',') => {
                    i += 1;
                    break;
                }
                Some(_) => {}
            }
        }
    }
}

/// `dispatchSecret`.
fn dispatch_secret(m: &instructions::Mount) -> Result<Secret, Fail> {
    let target_path = m.target.clone();
    let mut id = m.id.clone();
    if !m.source.is_empty() {
        id = m.source.clone();
    }
    if id.is_empty() {
        if target_path.is_empty() {
            return Err(Fail::new(b"one of source, target required".to_vec()));
        }
        id = path_base(&target_path);
    }
    let mut target = (!target_path.is_empty()).then(|| target_path.clone());
    if m.env.is_none() {
        let dest = if target_path.is_empty() {
            errb(&[b"/run/secrets/", &path_base(&id)])
        } else {
            target_path.clone()
        };
        target = Some(dest);
    }
    let (mut uid, mut gid, mut mode) = (0, 0, 0o400);
    if m.uid.is_some() || m.gid.is_some() || m.mode.is_some() {
        uid = u32::try_from(m.uid.unwrap_or(0)).unwrap_or(0);
        gid = u32::try_from(m.gid.unwrap_or(0)).unwrap_or(0);
        mode = u32::try_from(m.mode.unwrap_or(0o400)).unwrap_or(0o400);
    }
    Ok(Secret {
        id,
        target,
        env: m.env.clone(),
        uid,
        gid,
        mode,
        optional: !m.required,
    })
}

/// `dispatchSSH`.
fn dispatch_ssh(m: &instructions::Mount) -> Result<Ssh, Fail> {
    if !m.source.is_empty() {
        return Err(Fail::new(b"ssh does not support source".to_vec()));
    }
    let (mut uid, mut gid, mut mode) = (0, 0, 0o600);
    if m.uid.is_some() || m.gid.is_some() || m.mode.is_some() {
        uid = u32::try_from(m.uid.unwrap_or(0)).unwrap_or(0);
        gid = u32::try_from(m.gid.unwrap_or(0)).unwrap_or(0);
        mode = u32::try_from(m.mode.unwrap_or(0o600)).unwrap_or(0o600);
    }
    Ok(Ssh {
        id: m.id.clone(),
        target: m.target.clone(),
        uid,
        gid,
        mode,
        optional: !m.required,
    })
}

/// `ChompHeredocContent`: tabs at the start of each line dropped.
fn chomp_heredoc(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut line_start = true;
    for &c in src {
        if line_start && c == b'\t' {
            continue;
        }
        line_start = c == b'\n';
        out.push(c);
    }
    out
}

/// `summarizeHeredoc`: the first line, and `...` if more follow.
fn summarize_heredoc(doc: &[u8]) -> Vec<u8> {
    let doc = go::trim_space(doc);
    let doc: Vec<u8> = {
        let mut out = Vec::with_capacity(doc.len());
        let mut i = 0;
        while let Some(&c) = doc.get(i) {
            if c == b'\r' && doc.get(i + 1) == Some(&b'\n') {
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
        }
        out
    };
    let mut lines = doc.split(|&b| b == b'\n');
    let first = lines.next().unwrap_or_default().to_vec();
    if lines.next().is_some() {
        errb(&[&first, b"..."])
    } else {
        first
    }
}

/// `normalizeContextPaths`: the paths sorted, relative; none when the whole context is.
fn normalize_context_paths(paths: &BTreeSet<Vec<u8>>) -> Option<Vec<Vec<u8>>> {
    if paths.is_empty() || paths.contains(b"/".as_slice()) {
        return None;
    }
    let mut out: Vec<Vec<u8>> = paths.iter().map(|p| go::join(&[b".", p])).collect();
    out.sort();
    Some(out)
}

/// A domain's directory: `TO`'s, which must be absolute, the anchor its grants and checks
/// are found by whatever order the file declares them in; else `/agents/<name>` or
/// `/harness/<name>` (§12.1).
fn domain_dir(dom: &crate::agentfile::Domain, harness: bool) -> Result<Vec<u8>, Vec<u8>> {
    match &dom.to {
        Some(to) if to.starts_with(b"/") => {
            let clean = go::clean(to);
            if clean == b"/" {
                return Err(b"TO / would make the whole image one domain: name a directory".to_vec());
            }
            Ok(clean)
        }
        Some(to) => Err(errb(&[
            b"TO ",
            to,
            b": a domain's directory must be absolute, as its grants and checks find it by it",
        ])),
        None => Ok(errb(&[
            if harness {
                b"/harness/".as_slice()
            } else {
                b"/agents/"
            },
            &dom.name,
        ])),
    }
}

/// `State.WithImageConfig`: the image's environment (each variable added), working
/// directory and platform.
fn with_image_config(state: &mut State, img: &Image) {
    for e in &img.config.env {
        let (k, v) = match e.iter().position(|&b| b == b'=') {
            Some(i) => (go::head(e, i), go::tail(e, i + 1)),
            None => (e.as_slice(), b"".as_slice()),
        };
        if !k.is_empty() {
            state.env.add(k, v);
        }
    }
    state.set_dir(&img.config.working_dir);
    if !img.platform.architecture.is_empty() && !img.platform.os.is_empty() {
        state.platform = Some(Platform {
            os: img.platform.os.clone(),
            architecture: img.platform.architecture.clone(),
            variant: img.platform.variant.clone(),
            os_version: img.platform.os_version.clone(),
            os_features: img.platform.os_features.clone(),
        });
    }
}

/// `emptyImage`.
fn empty_image(p: &Platform) -> Image {
    Image {
        platform: p.clone(),
        rootfs: crate::image::RootFs {
            kind: b"layers".to_vec(),
            diff_ids: None,
        },
        config: crate::image::Config {
            working_dir: b"/".to_vec(),
            env: vec![[b"PATH=".as_slice(), DEFAULT_PATH].concat()],
            ..crate::image::Config::default()
        },
        ..Image::default()
    }
}

/// `parsePort`: `[ip:][host:]port[-end][/proto]` as the `port/proto` it exposes.
fn parse_port(raw: &[u8], loc: &Location, lint: &LinterView<'_>) -> Result<Vec<Vec<u8>>, Fail> {
    let parts: Vec<&[u8]> = raw.split(|&b| b == b':').collect();
    let (ip, host, container): (Vec<u8>, &[u8], &[u8]) = match parts.as_slice() {
        [c] => (Vec::new(), b"", c),
        [h, c] => (Vec::new(), h, c),
        [i, h, c] => (i.to_vec(), h, c),
        _ => {
            let n = parts.len();
            let ip = parts.get(..n - 2).unwrap_or_default().join(&b':');
            (
                ip,
                parts.get(n - 2).copied().unwrap_or_default(),
                parts.get(n - 1).copied().unwrap_or_default(),
            )
        }
    };
    let (port, proto) = match container.iter().position(|&b| b == b'/') {
        Some(at) => (go::head(container, at), go::tail(container, at + 1)),
        None => (container, &b""[..]),
    };
    if port.is_empty() {
        return Err(Fail::new(errb(&[
            b"invalid port: ",
            go::quote(raw).as_bytes(),
            b": no port specified",
        ])));
    }
    let proto: Vec<u8> = match go::to_lower(proto).as_slice() {
        b"" => b"tcp".to_vec(),
        b"tcp" | b"udp" | b"sctp" => proto.to_vec(),
        _ => {
            return Err(Fail::new(errb(&[
                b"invalid port: ",
                go::quote(raw).as_bytes(),
                b": invalid proto: ",
                proto,
            ])));
        }
    };
    if proto != go::to_lower(&proto) {
        let msg = errb(&[
            b"Defined protocol '",
            raw,
            b"' in EXPOSE instruction should be lowercase",
        ]);
        lint.run(&lint::EXPOSE_PROTO_CASING, loc, Some(&msg));
    }
    if !ip.is_empty() || !host.is_empty() {
        let msg = errb(&[
            b"EXPOSE instruction should not define an IP address or host-port mapping, found '",
            raw,
            b"'",
        ]);
        lint.run(&lint::EXPOSE_INVALID_FORMAT, loc, Some(&msg));
    }
    let mut ip = ip;
    if ip.first() == Some(&b'[') {
        // net.SplitHostPort(ip + ":"): the brackets stripped.
        match ip.iter().position(|&b| b == b']') {
            Some(end) if end + 1 == ip.len() => ip = go::span(&ip, 1, end).to_vec(),
            _ => {
                return Err(Fail::new(errb(&[b"invalid IP address ", &ip])));
            }
        }
    }
    if !ip.is_empty() && !valid_ip(&ip) {
        return Err(Fail::new(errb(&[b"invalid IP address: ", &ip])));
    }
    let (start, end) = port_range(port).map_err(|_| Fail::new(errb(&[b"invalid containerPort: ", port])))?;
    if !host.is_empty() {
        let (hs, he) = port_range(host).map_err(|_| Fail::new(errb(&[b"invalid hostPort: ", host])))?;
        if end - start != he - hs && end != start {
            return Err(Fail::new(errb(&[
                b"invalid ranges specified for container and host Ports: ",
                port,
                b" and ",
                host,
            ])));
        }
    }
    let proto = go::to_lower(&proto);
    Ok((start..=end)
        .map(|p| errb(&[p.to_string().as_bytes(), b"/", &proto]))
        .collect())
}

fn port_range(s: &[u8]) -> Result<(u32, u32), ()> {
    if s.is_empty() {
        return Err(());
    }
    let (a, b) = match s.iter().position(|&c| c == b'-') {
        Some(at) => (go::head(s, at), Some(go::tail(s, at + 1))),
        None => (s, None),
    };
    let start = port_number(a)?;
    match b {
        None => Ok((start, start)),
        Some(b) if b == a => Ok((start, start)),
        Some(b) => {
            let end = port_number(b)?;
            if end < start { Err(()) } else { Ok((start, end)) }
        }
    }
}

fn port_number(s: &[u8]) -> Result<u32, ()> {
    let n = go_atoi(s).ok_or(())?;
    u32::try_from(n).ok().filter(|&n| n <= 65535).ok_or(())
}

/// `net.ParseIP`: dotted IPv4, or IPv6 with `::` at most once and an IPv4 tail.
fn valid_ip(s: &[u8]) -> bool {
    std::str::from_utf8(s)
        .ok()
        .is_some_and(|t| t.parse::<std::net::Ipv4Addr>().is_ok() || t.parse::<std::net::Ipv6Addr>().is_ok())
}

trait LintError {
    fn error_message(&self) -> Vec<u8>;
}

impl LintError for Linter {
    /// `Linter.Error`: the rules that warned, each once, in the order they first did.
    fn error_message(&self) -> Vec<u8> {
        let mut seen: Vec<&str> = Vec::new();
        for w in self.warnings() {
            if !seen.contains(&w.rule) {
                seen.push(w.rule);
            }
        }
        format!("lint violation found for rules: {}", seen.join(", ")).into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves every base image as one of one layer for linux/amd64, answers
    /// SOURCE_DATE_EPOCH's source as told, and keeps what it was asked.
    struct Times {
        asked: std::cell::RefCell<Vec<EpochSource>>,
        answer: Result<Option<(i64, u32)>, Vec<u8>>,
        logged: std::cell::RefCell<Vec<String>>,
    }

    impl Resolver for Times {
        fn resolve(&self, name: &[u8], _: &Platform, log: &[u8]) -> Result<Resolved, Vec<u8>> {
            self.logged
                .borrow_mut()
                .push(String::from_utf8_lossy(log).into_owned());
            Ok(Resolved {
                reference: name.to_vec(),
                digest: None,
                // A layer, or it is scratch, as BuildKit takes an image of none.
                config: br#"{"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":["sha256:24454f830cdb571e2c4ad15481119c43b3cafd48dd869a9b2945d1036d1dc68d"]}}"#.to_vec(),
            })
        }

        fn epoch(&self, source: &EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>> {
            self.asked.borrow_mut().push(source.clone());
            self.answer.clone()
        }
    }

    fn planned(
        text: &str,
        args: &[(&str, &str)],
        answer: Result<Option<(i64, u32)>, Vec<u8>>,
    ) -> (Result<Plan, Error>, Vec<EpochSource>, Vec<String>) {
        let opts = Options {
            target_platform: Platform::new("linux", "amd64"),
            build_args: args
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
            ..Default::default()
        };
        let times = Times {
            asked: Default::default(),
            answer,
            logged: Default::default(),
        };
        let planned = plan(text.as_bytes(), &opts, &times);
        (planned, times.asked.into_inner(), times.logged.into_inner())
    }

    const SUM: &str = "sha256:24454f830cdb571e2c4ad15481119c43b3cafd48dd869a9b2945d1036d1dc68d";

    /// SOURCE_DATE_EPOCH naming a stage asks for the time of its one remote ADD, as the
    /// stage's ARGs and the build args make it, without SOURCE_DATE_EPOCH itself; the time
    /// answered is the build's, to the nanosecond in its history and its WORKDIRs, and to
    /// the second where the exporter takes it (dockerfile/1.27.1 epoch.go, convert.go).
    #[test]
    fn a_source_stage_gives_the_build_its_time() {
        let text = format!(
            "ARG SOURCE_DATE_EPOCH\nFROM scratch AS Src\nARG V=1.${{SOURCE_DATE_EPOCH}}0\nARG W\n\
             ADD --checksum={SUM} https://example.com/v${{V}}/app${{W}}.tar.gz?x=1 /\n\n\
             FROM scratch\nWORKDIR /w\n"
        );
        let args = [("SOURCE_DATE_EPOCH", "SRC"), ("W", "-b")];
        let (planned, asked, _) = planned(&text, &args, Ok(Some((1_700_000_000, 5))));
        let planned = planned.unwrap();
        assert_eq!(
            asked,
            [EpochSource::Http {
                stage: b"src".to_vec(),
                url: b"https://example.com/v1.0/app-b.tar.gz?x=1".to_vec(),
                checksum: Some(SUM.as_bytes().to_vec()),
                filename: b"app-b.tar.gz".to_vec(),
            }]
        );
        assert_eq!(planned.epoch, Some(1_700_000_000));
        let created: Vec<_> = planned.image.history.iter().map(|h| h.created).collect();
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].map(|t| (t.unix(), t.nanosecond)),
            Some(((1_700_000_000, 5), 5))
        );
        let made: Vec<Option<i64>> = planned
            .graph
            .vertices
            .iter()
            .filter_map(|v| match &v.kind {
                llb::Kind::File { actions, .. } => Some(actions),
                _ => None,
            })
            .flatten()
            .filter_map(|a| match a {
                Action::Mkdir { created, .. } => Some(*created),
                _ => None,
            })
            .collect();
        assert_eq!(made, [Some(1_700_000_000_000_000_005)]);
    }

    /// `context` asks for the context's time; a Git source, for its repository's, the
    /// ADD's checksum in place of the URL's; a number asks for nothing; and what the
    /// resolver cannot answer fails the build with what it says.
    #[test]
    fn each_source_of_the_time_is_asked_for_as_it_is_named() {
        let (_, asked, _) = planned("FROM scratch\n", &[("SOURCE_DATE_EPOCH", "context")], Ok(None));
        assert_eq!(asked, [EpochSource::Context]);
        let text = "FROM scratch AS src\nADD --checksum=abc123 https://github.com/moby/buildkit.git?ref=v1 /\n\nFROM scratch\n";
        let (_, asked, _) = planned(text, &[("SOURCE_DATE_EPOCH", "src")], Ok(None));
        let [EpochSource::Git { stage, git }] = asked.as_slice() else {
            panic!("{asked:?}");
        };
        assert_eq!(
            (stage.as_slice(), git.checksum.as_slice()),
            (&b"src"[..], &b"abc123"[..])
        );
        assert_eq!(git.remote, b"https://github.com/moby/buildkit.git");
        let (_, asked, _) = planned("FROM scratch\n", &[("SOURCE_DATE_EPOCH", "+42")], Ok(None));
        assert!(asked.is_empty());
        let (failed, _, _) = planned(
            "FROM scratch\n",
            &[("SOURCE_DATE_EPOCH", "context")],
            Err(b"no answer".to_vec()),
        );
        assert_eq!(failed.err().map(|e| e.message), Some(b"no answer".to_vec()));
    }

    /// The CopyIgnoredFile warnings planning `text` with `ignore` as the .dockerignore.
    fn ignored(text: &str, ignore: &[&str]) -> Vec<String> {
        let opts = Options {
            target_platform: Platform::new("linux", "amd64"),
            excludes: ignore.iter().map(|p| p.as_bytes().to_vec()).collect(),
            ..Default::default()
        };
        let times = Times {
            asked: Default::default(),
            answer: Ok(None),
            logged: Default::default(),
        };
        let planned = plan(text.as_bytes(), &opts, &times).unwrap();
        planned
            .warnings
            .iter()
            .filter(|w| w.rule == "CopyIgnoredFile")
            .map(|w| format!("{} {:?}", String::from_utf8_lossy(&w.message), w.location))
            .collect()
    }

    /// What ADD reads, and COPY from the context, is warned of where the .dockerignore
    /// excludes it or a directory above it, its name cleaned (validateCopySourcePath);
    /// nothing is said of a stage's files, of anything once a pattern excludes from the
    /// exclusions, or of the context's root unless a pattern excludes all of it.
    #[test]
    fn copying_what_the_dockerignore_excludes_is_warned_of() {
        let text = "FROM scratch AS s\nFROM scratch\nCOPY ./docs/../README.md /\nADD secret/key notes.txt /\n\
                    COPY --from=s README.md /\nCOPY . /\n";
        assert_eq!(
            ignored(text, &["*.md", "secret"]),
            [
                "Attempting to Copy file \"README.md\" that is excluded by .dockerignore [(3, 3)]",
                "Attempting to Add file \"secret/key\" that is excluded by .dockerignore [(4, 4)]",
            ]
        );
        assert!(ignored(text, &["*.md", "secret", "!keep.md"]).is_empty());
        assert_eq!(
            ignored(text, &["**"]),
            [
                "Attempting to Copy file \"README.md\" that is excluded by .dockerignore [(3, 3)]",
                "Attempting to Add file \"secret/key\" that is excluded by .dockerignore [(4, 4)]",
                "Attempting to Add file \"notes.txt\" that is excluded by .dockerignore [(4, 4)]",
                "Attempting to Copy file \".\" that is excluded by .dockerignore [(6, 6)]",
            ]
        );
        assert!(ignored(text, &[]).is_empty());
        // `.` matches the root, yet excludes nothing of it (measured, dockerfile/1.27.1).
        assert!(ignored("FROM scratch\nCOPY . /\nCOPY / /\n", &["."]).is_empty());
    }

    /// The frontend a build is planned with: any tag of docker/dockerfile, labs ones and
    /// upstream builds included, is this one, named by `# syntax=`, `//syntax=` or
    /// BUILDKIT_SYNTAX, which wins; another fails the build where it is named, a `#`
    /// directive's line counted past a shebang.
    #[test]
    fn a_frontend_shards_cannot_run_is_refused_where_it_is_named() {
        let plans = |text: &str, args: &[(&str, &str)]| {
            planned(text, args, Ok(None))
                .0
                .map(|_| ())
                .map_err(|e| (String::from_utf8_lossy(&e.message).into_owned(), e.location))
        };
        for text in [
            "# syntax=docker/dockerfile:1\nFROM scratch\n",
            "# syntax = docker.io/docker/dockerfile:1.4-labs --x\nFROM scratch\n",
            "#syntax=docker/dockerfile-upstream:master@sha256:24454f830cdb571e2c4ad15481119c43b3cafd48dd869a9b2945d1036d1dc68d\nFROM scratch\n",
            "# check=skip=all\n# syntax=docker/dockerfile\nFROM scratch\n",
        ] {
            assert_eq!(plans(text, &[]), Ok(()), "{text}");
        }
        let refused = |r: &str| {
            format!(
                "shards cannot run frontend {r}: it builds Dockerfiles with its own port of docker/dockerfile 1.27.1"
            )
        };
        assert_eq!(
            plans(
                "#!/bin/frontend\n# syntax=myorg/frontend:1 --flag\nFROM scratch\n",
                &[]
            ),
            Err((refused("myorg/frontend:1"), vec![vec![(2, 2)]]))
        );
        assert_eq!(
            plans("// syntax=ghcr.io/docker/dockerfile:1\nFROM scratch\n", &[]),
            Err((refused("ghcr.io/docker/dockerfile:1"), vec![vec![(1, 1)]]))
        );
        assert_eq!(
            plans("{\"syntax\": \"docker/dockerfile:1\", \"syntax\": \"x/y\"}", &[]),
            Err((refused("x/y"), vec![vec![(0, 0)]]))
        );
        assert_eq!(
            plans(
                "# syntax=x/y\nFROM scratch\n",
                &[("BUILDKIT_SYNTAX", " docker/dockerfile:1 ")]
            ),
            Ok(())
        );
        assert_eq!(
            plans("FROM scratch\n", &[("BUILDKIT_SYNTAX", "x/y z")]),
            Err((
                format!(
                    "failed with build-arg:BUILDKIT_SYNTAX = x/y z: {}",
                    refused("x/y")
                ),
                vec![]
            ))
        );
    }

    /// The build args dockerui reads as options, each as BuildKit reads it (measured,
    /// dockerfile/1.27.1): BUILDKIT_MULTI_PLATFORM names steps by platform, and fails as
    /// no boolean; BUILDKIT_DOCKERFILE_CHECK stands in for `# check=`, and fails as no
    /// check; BUILDKIT_SANDBOX_HOSTNAME names RUN's host; BUILDKIT_CACHE_MOUNT_NS is what
    /// cache mounts' IDs are under.
    #[test]
    fn buildkits_build_arg_options_are_read_as_dockerui_reads_them() {
        let message = |text: &str, args: &[(&str, &str)]| {
            planned(text, args, Ok(None))
                .0
                .err()
                .map(|e| String::from_utf8_lossy(&e.message).into_owned())
        };
        assert_eq!(
            message("FROM scratch\n", &[("BUILDKIT_MULTI_PLATFORM", "x")]).as_deref(),
            Some("invalid boolean value for multi-platform: x")
        );
        assert_eq!(
            message("FROM scratch\n", &[("BUILDKIT_DOCKERFILE_CHECK", "nope")]).as_deref(),
            Some("failed to parse build-arg:BUILDKIT_DOCKERFILE_CHECK: invalid check option \"nope\"")
        );
        assert_eq!(
            message("FROM scratch\n", &[("BUILDKIT_DOCKERFILE_CHECK", "error=maybe")]).as_deref(),
            Some(
                "failed to parse build-arg:BUILDKIT_DOCKERFILE_CHECK: failed to parse check option \"error=maybe\": \
                 strconv.ParseBool: parsing \"maybe\": invalid syntax"
            )
        );
        let casing = "# check=skip=all\nFROM scratch\nRUN true\nrun true\n";
        assert_eq!(message(casing, &[]), None);
        assert_eq!(
            message(casing, &[("BUILDKIT_DOCKERFILE_CHECK", "error=true")]).as_deref(),
            Some("lint violation found for rules: ConsistentInstructionCasing")
        );
        // A base image's platform names steps; scratch has none, which names none (measured).
        let text = "FROM alpine\nRUN --mount=type=cache,target=/c true\n";
        let args = [
            ("BUILDKIT_MULTI_PLATFORM", "1"),
            ("BUILDKIT_SANDBOX_HOSTNAME", "myhost"),
            ("BUILDKIT_CACHE_MOUNT_NS", "ns"),
        ];
        // The cache mount, from no stage, is from scratch, a stage of its own: so the one
        // stage written keeps its name (measured, as each name here).
        for (args, hostname, id, prefix, log) in [
            (
                &args[..],
                "myhost",
                "ns//c",
                "[linux/amd64 stage-0 2/2] RUN",
                "[linux/amd64 internal] load metadata for docker.io/library/alpine:latest",
            ),
            (
                &[],
                "",
                "//c",
                "[stage-0 2/2] RUN",
                "[internal] load metadata for docker.io/library/alpine:latest",
            ),
        ] {
            let (planned, _, logged) = planned(text, args, Ok(None));
            assert_eq!(logged, [log]);
            let planned = planned.unwrap();
            let (run, process, mounts) = planned
                .graph
                .vertices
                .iter()
                .find_map(|v| match &v.kind {
                    llb::Kind::Exec { process, mounts, .. } => Some((v, process, mounts)),
                    _ => None,
                })
                .unwrap();
            assert_eq!(process.hostname, hostname.as_bytes());
            assert!(
                mounts
                    .iter()
                    .any(|m| matches!(&m.kind, llb::MountKind::Cache { id: i, .. } if i == id.as_bytes())),
                "{mounts:?}"
            );
            let name = run.meta.description.get(b"llb.customname".as_slice()).unwrap();
            assert!(
                name.starts_with(prefix.as_bytes()),
                "{}",
                String::from_utf8_lossy(name)
            );
        }
    }

    /// A platform the environment names in part is completed from the platform the
    /// frontend runs on, the build's (containerd's platforms.Parse), not from any other.
    #[test]
    fn a_partial_target_platform_is_completed_from_the_builds() {
        let opts = Options {
            target_platform: Platform::new("linux", "amd64"),
            build_platforms: vec![Platform::new("linux", "arm64")],
            build_args: [("BUILDKIT_MULTI_PLATFORM", "1"), ("TARGETPLATFORM", "linux")]
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
            ..Default::default()
        };
        let times = Times {
            asked: Default::default(),
            answer: Ok(None),
            logged: Default::default(),
        };
        let planned = plan(b"FROM alpine\nARG TARGETPLATFORM\nRUN true\n", &opts, &times).unwrap();
        let names: Vec<String> = planned
            .graph
            .vertices
            .iter()
            .filter_map(|v| v.meta.description.get(b"llb.customname".as_slice()))
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("[linux/amd64->arm64 ") && n.ends_with("] RUN true")),
            "{names:?}"
        );
    }

    /// A target other than Linux is refused: shards' guests are Linux.
    #[test]
    fn a_target_that_is_not_linux_is_refused() {
        let opts = Options {
            target_platform: Platform::new("windows", "amd64"),
            ..Default::default()
        };
        let times = Times {
            asked: Default::default(),
            answer: Ok(None),
            logged: Default::default(),
        };
        let e = plan(b"FROM scratch\n", &opts, &times).unwrap_err();
        assert_eq!(
            String::from_utf8_lossy(&e.message),
            "shards builds Linux guests: the target platform windows/amd64 is not one"
        );
    }

    /// `strings.Index`: an empty needle is at the start.
    #[test]
    fn an_empty_needle_is_found_at_the_start() {
        assert_eq!(find(b"abc", b""), Some(0));
        assert_eq!(find(b"", b""), Some(0));
        assert_eq!(find(b"abc", b"c"), Some(2));
        assert_eq!(find(b"abc", b"d"), None);
    }
}
