//! Agentfile builds for orchestrators (docs/design/architecture.md D263): [`Builds::build`]
//! builds an image from a context and its Agentfile (or Dockerfile) into the caller's home's
//! store (`SHARDS_HOME`, else the default), as `shards build image` builds one, and gives back
//! its image's ID, pinned ([`Pinned`]), which [`crate::image::ImageSpec::image`] boots.
//!
//! It starts `shards api-build`, an internal command, one process of the `shards` binary for
//! the build, which runs the build in itself, with no daemon: its builder microVMs are its
//! own, in its process group, which ends with it ([`crate::command`]). The request goes on its
//! stdin; its steps' progress ([`BuildEvent`]) and its end come back as frames on its stdout
//! ([`wire`]). Secrets and SSH are given by reference, as the CLI takes them (D51, D68): a
//! file's path or a variable's name of the caller's environment, which the build reads, and a
//! key's path or the agent at `SSH_AUTH_SOCK`; no secret's bytes cross this channel.
//!
//! [`Builds::component`] builds one `AGENT`, `HARNESS` or `MCP` install alone (D270), from its
//! directive's own source, into an OSI artifact in the same store ([`ComponentSpec`]): the
//! build of an Agentfile of that one directive over `scratch`, so its fetch, pins, grants and
//! policies are a whole build's. Its digest is what a running image's members are swapped to.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::command::{Command, NotStarted};
use crate::image::wire::{Reader, WireError, Writer, frame};

/// Where builds run: which `shards` (else `SHARDS_BIN`, else `shards` on `PATH`) and which
/// home's store they write to (else this process's `SHARDS_HOME`, else the default).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Builds {
    pub binary: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// Variables of the build's own process, over this process's (`home`'s `SHARDS_HOME` over
    /// these): where a [`SecretSource::Env`] is read, so a secret this process holds reaches
    /// its build without being set in this process's environment or written to a file.
    pub env: Vec<(OsString, OsString)>,
}

/// A build to make.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildSpec {
    /// Its context: a directory, by its absolute path.
    pub context: PathBuf,
    /// Its Agentfile or Dockerfile, by its absolute path; none for the context's own
    /// (`Agentfile`, else `Dockerfile`), as `shards build image` finds it.
    pub file: Option<PathBuf>,
    /// The platform it builds for, as OCI names one (`linux/arm64`); none for this host's.
    pub platform: Option<String>,
    /// The name it is tagged with in the store, beside its ID.
    pub tag: Option<String>,
    /// Its secrets, each by reference (`RUN --mount=type=secret`, D51).
    pub secrets: Vec<BuildSecret>,
    /// Its SSH agents (`RUN --mount=type=ssh`, D68).
    pub ssh: Vec<BuildSsh>,
}

/// A secret a build's steps may mount, by its id, read by the build where it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSecret {
    pub id: String,
    pub source: SecretSource,
}

/// Where a secret's bytes are: a file, or a variable of the caller's environment, which the
/// build inherits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    File(PathBuf),
    Env(String),
}

/// An SSH agent a build's steps may reach, by its id: the keys at `paths`, served by the
/// build, or, where none, the agent at the caller's `SSH_AUTH_SOCK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSsh {
    pub id: String,
    pub paths: Vec<PathBuf>,
}

/// A built image: its ID (`sha256:<hex>`, the digest of its index or manifest), which names it
/// in its store whatever its tags are. A built component's is its OSI artifact's manifest's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub digest: String,
}

/// What a component installs, as its directive and its OSI artifact's type say it (D54, D57).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    Agent,
    Harness,
    /// An MCP server spoken to over stdio: one a member's harness runs from its directory.
    Mcp,
}

impl ComponentKind {
    /// Its directive's word in an Agentfile.
    pub fn directive(self) -> &'static str {
        match self {
            Self::Agent => "AGENT",
            Self::Harness => "HARNESS",
            Self::Mcp => "MCP",
        }
    }

    /// The directory its directive lays it in where no `TO` says another (§12.1), beneath
    /// which its name is its own.
    pub fn parent(self) -> &'static str {
        match self {
            Self::Agent => "/agents",
            Self::Harness => "/harness",
            Self::Mcp => "/mcp",
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Agent => 0,
            Self::Harness => 1,
            Self::Mcp => 2,
        }
    }

    fn of_tag(tag: u8) -> Result<ComponentKind, WireError> {
        Ok(match tag {
            0 => Self::Agent,
            1 => Self::Harness,
            2 => Self::Mcp,
            tag => {
                return Err(WireError::Tag {
                    what: "a component's kind",
                    tag,
                });
            }
        })
    }
}

/// An `s3://` source's own flags (D54): where its store is and which version of the object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct S3Source {
    /// `--endpoint=URL`: an S3-compatible store's (R2, Mantle), else AWS's for its region.
    pub endpoint: Option<String>,
    /// `--region=R`.
    pub region: Option<String>,
    /// `--path-style`.
    pub path_style: bool,
    /// `--version-id=V`: the object's version, immutable in S3.
    pub version_id: Option<String>,
    /// `--credentials=aws-default`: signed through the host's AWS default chain, which the
    /// build names on its step; else by the build's secrets, else unsigned (D54).
    pub aws_default_credentials: bool,
}

/// One `AGENT`, `HARNESS` or `MCP` install to build alone (D270): its directive's name, source
/// and flags, as an Agentfile would say them, built as that directive is in a whole build, so
/// that a source the build refuses unpinned (an http(s) or s3 source without `--checksum` or
/// `--version-id`) is refused here in the build's own words. Its content and normalized startup
/// become an OSI artifact (D57) kept in the store as `tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSpec {
    pub kind: ComponentKind,
    /// Its name, as the directive declares it (`^[a-z][a-z0-9-_.]*$`).
    pub name: String,
    /// Its source, as BuildKit reads one (D54): a Git remote (`https://…/REPO.git#REF`,
    /// `ssh://…`, `git://…`, `git@HOST:…`), an http(s) URL, or an `s3://BUCKET/KEY` object; a
    /// context path or an OCI reference is no source a component is built of.
    pub source: String,
    /// `--checksum=sha256:<hex>`.
    pub checksum: Option<String>,
    /// Each `--redirect=HOST[:PORT]` the fetch is granted besides its source's origin.
    pub redirects: Vec<String>,
    pub s3: S3Source,
    /// The platform it is built for, as OCI names one (`linux/arm64`); none for this host's.
    pub platform: Option<String>,
    /// The name it is kept by in the store beside its digest, as `shards build agent -t` names
    /// one.
    pub tag: String,
    /// The secrets its fetch may use, by reference (`GIT_AUTH_TOKEN`, `AWS_ACCESS_KEY_ID`, …).
    pub secrets: Vec<BuildSecret>,
    /// Its fetch's SSH agents (a `git@…` remote).
    pub ssh: Vec<BuildSsh>,
}

/// A build's progress, step by step, as `shards build image` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildEvent {
    /// Step `index` began (or began again, under `name`).
    Step { index: u32, name: String },
    /// A line step `index` said: its log or its own word.
    Line { index: u32, text: String },
    /// Step `index` ended.
    Ended { index: u32, how: StepEnd },
}

/// How a step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepEnd {
    Done,
    /// Its build cache answered it.
    Cached,
    /// It failed, in the build's words.
    Failed(String),
    /// The build's failure stopped it.
    Canceled,
}

/// Why a build made no image. Where its error is at lines of its Agentfile, `lines` holds each
/// range of them (1-based, first to last), as `shards build image` shows them in its excerpt:
/// a syntax error's line, a failed step's instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildRefusal {
    /// The spec, refused before anything started: which part, and why.
    Spec { what: &'static str, why: String },
    /// A build policy's decision (D101) refused one of its sources: what the policy said (its
    /// deny messages, none where it gave none), and the build's error.
    Policy {
        said: Vec<String>,
        lines: Vec<RangeInclusive<u32>>,
        message: String,
    },
    /// A step failed: its number and name, its own error, and the build's.
    Step {
        index: u32,
        name: String,
        error: String,
        message: String,
        lines: Vec<RangeInclusive<u32>>,
    },
    /// The build failed outside any step (its Agentfile's text, its plan, a base image's
    /// pull, its export), in shards' words.
    Failed {
        message: String,
        lines: Vec<RangeInclusive<u32>>,
    },
    /// The `shards` binary did not start: its path and the OS's words.
    Start(String),
    /// What this process could not make for it.
    Os {
        what: &'static str,
        kind: std::io::ErrorKind,
    },
    /// It ended without its end said: its exit status where it was reaped, and its last
    /// diagnostics.
    Ended { status: Option<i32>, said: String },
    /// An answer this side does not read (another shards'; a request this side's host did
    /// not read is said as its [`Failed`](Self::Failed)).
    Wire(WireError),
}

impl std::fmt::Display for BuildRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spec { what, why } => write!(f, "{what}: {why}"),
            // As buildx says a refusal: each of the policy's messages, then the error.
            Self::Policy { said, message, .. } => {
                for m in said {
                    writeln!(f, "Policy: {m}")?;
                }
                f.write_str(message)
            }
            Self::Step { message, .. } | Self::Failed { message, .. } => f.write_str(message),
            Self::Start(why) => write!(f, "starting shards api-build: {why}"),
            Self::Os { what, kind } => write!(f, "{what}: {kind}"),
            Self::Ended { status, said } => {
                f.write_str("shards api-build ended without its build's end")?;
                if let Some(status) = status {
                    write!(f, " (status {status})")?;
                }
                if !said.is_empty() {
                    write!(f, ": {}", said.trim_end())?;
                }
                Ok(())
            }
            Self::Wire(e) => write!(f, "shards api-build's answer: {e}"),
        }
    }
}

impl std::error::Error for BuildRefusal {}

impl From<NotStarted> for BuildRefusal {
    fn from(e: NotStarted) -> BuildRefusal {
        match e {
            NotStarted::Start(why) => BuildRefusal::Start(why),
            NotStarted::Os { what, kind } => BuildRefusal::Os { what, kind },
        }
    }
}

impl Builds {
    /// Builds `spec` and gives back its image, pinned, each step's progress told to `events`
    /// as it comes; or why no image was made, typed. Dropped mid-build (a panic in `events`),
    /// its `shards api-build` and every builder microVM of it are killed.
    pub fn build(
        &self,
        spec: &BuildSpec,
        events: &mut dyn FnMut(BuildEvent),
    ) -> Result<Pinned, BuildRefusal> {
        let request = wire::Request::of(spec)?;
        self.follow(wire::request(&request).map_err(BuildRefusal::Wire), events)
    }

    /// Builds `spec`'s one install alone (D270) and gives back its OSI artifact, pinned by its
    /// manifest's digest and kept in the store as its tag, each step's progress told to
    /// `events`; or why none was made, typed as [`build`](Self::build)'s are: a spec this side
    /// refuses before anything starts as [`BuildRefusal::Spec`], a source the build refuses
    /// (unpinned, not granted, of a kind its directive takes none of) in the build's words.
    pub fn component(
        &self,
        spec: &ComponentSpec,
        events: &mut dyn FnMut(BuildEvent),
    ) -> Result<Pinned, BuildRefusal> {
        let request = wire::ComponentRequest::of(spec)?;
        self.follow(
            wire::component_request(&request).map_err(BuildRefusal::Wire),
            events,
        )
    }

    /// Starts `shards api-build`, sends it `frame`, its one request, and follows it to its end.
    fn follow(
        &self,
        frame: Result<Vec<u8>, BuildRefusal>,
        events: &mut dyn FnMut(BuildEvent),
    ) -> Result<Pinned, BuildRefusal> {
        let frame = frame?;
        let env: Vec<(OsString, OsString)> = self
            .env
            .iter()
            .cloned()
            .chain(
                self.home
                    .iter()
                    .map(|home| (OsString::from("SHARDS_HOME"), home.clone().into_os_string())),
            )
            .collect();
        let mut command = Command::start(self.binary.as_deref(), &[OsStr::new("api-build")], &env)?;
        let asked = {
            let os = |e: std::io::Error| BuildRefusal::Os {
                what: "the build's request",
                kind: e.kind(),
            };
            command
                .input()
                .ok_or(BuildRefusal::Os {
                    what: "the build's request",
                    kind: std::io::ErrorKind::BrokenPipe,
                })
                .and_then(|input| input.write_all(&frame).map_err(os))
        };
        // Its request is whole: nothing more goes its way.
        command.close();
        let mut steps = Steps::default();
        let outcome = asked.and_then(|()| {
            loop {
                let (kind, payload) = match command.frame() {
                    Ok(Some(frame)) => frame,
                    Ok(None) | Err(_) => break Ok(None),
                };
                match wire::answer(kind, &payload)? {
                    wire::Answer::Event(event) => {
                        steps.saw(&event);
                        events(event);
                    }
                    wire::Answer::Policy(said) if steps.policy.is_none() => steps.policy = Some(said),
                    wire::Answer::At(lines) if steps.lines.is_none() => steps.lines = Some(lines),
                    wire::Answer::Policy(_) | wire::Answer::At(_) => {}
                    wire::Answer::Built(digest) => break Ok(Some(Ok(Pinned { digest }))),
                    wire::Answer::Failed(message) => break Ok(Some(Err(steps.refusal(message)))),
                }
            }
        });
        let ended = command.end();
        match outcome? {
            Some(end) => end,
            None => Err(BuildRefusal::Ended {
                status: ended.status,
                said: ended.said,
            }),
        }
    }
}

/// The steps a build's events named, the first that failed, and what the build said of its
/// error before its end: what its refusal says.
#[derive(Debug, Default)]
struct Steps {
    names: BTreeMap<u32, String>,
    failed: Option<(u32, String)>,
    /// What a policy's decision said as it refused a source, where one did.
    policy: Option<Vec<String>>,
    /// The lines of the Agentfile the error is at, as first told.
    lines: Option<Vec<RangeInclusive<u32>>>,
}

impl Steps {
    fn saw(&mut self, event: &BuildEvent) {
        match event {
            BuildEvent::Step { index, name } => {
                self.names.insert(*index, name.clone());
            }
            BuildEvent::Ended {
                index,
                how: StepEnd::Failed(error),
            } if self.failed.is_none() => self.failed = Some((*index, error.clone())),
            BuildEvent::Line { .. } | BuildEvent::Ended { .. } => {}
        }
    }

    /// A policy's refusal is the build's whatever step it failed (its policies' own), as buildx
    /// says it; else the first step that failed; else the build's failure outside a step.
    fn refusal(&mut self, message: String) -> BuildRefusal {
        let lines = self.lines.take().unwrap_or_default();
        if let Some(said) = self.policy.take() {
            return BuildRefusal::Policy { said, lines, message };
        }
        match self.failed.take() {
            Some((index, error)) => BuildRefusal::Step {
                index,
                name: self.names.get(&index).cloned().unwrap_or_default(),
                error,
                message,
                lines,
            },
            None => BuildRefusal::Failed { message, lines },
        }
    }
}

/// The channel between [`Builds::build`] and `shards api-build`: frames on its stdin (the
/// request, [`REQUEST`](to_host::REQUEST)) and its stdout (events, then its end), with the run
/// protocol's header, as an image run's channel (`crate::image::wire`) frames them.
pub mod wire {
    use super::*;

    /// The channel's version, its request's first byte, which a host refuses unless its own:
    /// no side is sent an answer it does not read. 2: [`POLICY`](to_client::POLICY) and
    /// [`AT`](to_client::AT).
    pub const VERSION: u8 = 2;

    /// What the client sends.
    pub mod to_host {
        /// The build to make ([`super::Request`]), the one frame the client sends.
        pub const REQUEST: u8 = 1;
        /// A component to build alone ([`super::ComponentRequest`], D270), in a request's
        /// place.
        pub const COMPONENT: u8 = 2;
    }

    /// What `shards api-build` answers.
    pub mod to_client {
        /// A step began: its index and name.
        pub const STEP: u8 = 1;
        /// A line of a step: its index and the line.
        pub const LINE: u8 = 2;
        /// A step ended: its index and how ([`super::StepEnd`]).
        pub const STEP_ENDED: u8 = 3;
        /// The build made its image: the image's ID. Its last frame.
        pub const BUILT: u8 = 4;
        /// The build failed: its error, in shards' words. Its last frame.
        pub const FAILED: u8 = 5;
        /// A policy's decision refused one of the build's sources: what it said, each message
        /// buildx prints as `Policy: …` (policysession.DenyMessages). Before its end.
        pub const POLICY: u8 = 6;
        /// The lines of the build's Agentfile its error is at, each range first and last, as
        /// BuildKit locates an error (solver/errdefs Source ranges). Before its end.
        pub const AT: u8 = 7;
    }

    /// A build's spec as `shards api-build` takes it: each path UTF-8, as the build's command
    /// line takes it.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Request {
        pub context: String,
        pub file: Option<String>,
        pub platform: Option<String>,
        pub tag: Option<String>,
        /// Each secret's id, and its file's path or its variable's name.
        pub secrets: Vec<(String, Source)>,
        /// Each agent's id, and its keys' paths.
        pub ssh: Vec<(String, Vec<String>)>,
    }

    /// Where a secret is, in the request.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Source {
        File(String),
        Env(String),
    }

    /// A component's spec as `shards api-build` takes it (D270): its directive's words, each
    /// one word of an Agentfile's line ([`ComponentRequest::directive`]), checked so on both
    /// sides of the channel ([`ComponentRequest::check`]).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ComponentRequest {
        pub kind: ComponentKind,
        pub name: String,
        pub source: String,
        pub checksum: Option<String>,
        pub redirects: Vec<String>,
        pub s3: S3Source,
        pub platform: Option<String>,
        pub tag: String,
        pub secrets: Vec<(String, Source)>,
        pub ssh: Vec<(String, Vec<String>)>,
    }

    /// A stage-style name, as an Agentfile names agents, harnesses and MCP servers
    /// (`^[a-z][a-z0-9-_.]*$`, D35).
    fn valid_name(name: &str) -> bool {
        let mut chars = name.chars();
        chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    }

    /// Why `word` is no single word of an Agentfile's line, as written: empty, or holding what
    /// the parser would read as more than the word (whitespace and control characters end a
    /// word or a line, `\` continues one, quotes group, `$` expands a variable, D56); none where
    /// it is one.
    fn unplain(word: &str) -> Option<&'static str> {
        if word.is_empty() {
            return Some("empty");
        }
        word.chars().find_map(|c| match c {
            c if c.is_whitespace() || c.is_control() => Some("whitespace or a control character"),
            '\\' => Some("`\\`, which continues a line"),
            '"' | '\'' | '`' => Some("a quote"),
            '$' => Some("`$`, which the directive expands"),
            _ => None,
        })
    }

    impl ComponentRequest {
        /// `spec` checked as a request, or refused, naming what of it, before anything starts.
        pub fn of(spec: &ComponentSpec) -> Result<ComponentRequest, BuildRefusal> {
            let (secrets, ssh) = access_of(&spec.secrets, &spec.ssh)?;
            let request = ComponentRequest {
                kind: spec.kind,
                name: spec.name.clone(),
                source: spec.source.clone(),
                checksum: spec.checksum.clone(),
                redirects: spec.redirects.clone(),
                s3: spec.s3.clone(),
                platform: spec.platform.clone(),
                tag: spec.tag.clone(),
                secrets,
                ssh,
            };
            request.check()?;
            Ok(request)
        }

        /// What both sides check of a request, as neither trusts the other's: its name an
        /// Agentfile's, each word of its directive one word of the line, one platform alone,
        /// and a tag. What its source is, and whether its flags go with it, is the build's to
        /// say, in its own words.
        pub fn check(&self) -> Result<(), BuildRefusal> {
            if !valid_name(&self.name) {
                return Err(refuse(
                    "the name",
                    format!(
                        "{:?}: no name an Agentfile gives: lowercase letters, digits, '-', '_' and '.', a letter first",
                        self.name
                    ),
                ));
            }
            let mut words: Vec<(&'static str, &str)> = vec![
                ("the source", self.source.as_str()),
                ("the tag", self.tag.as_str()),
            ];
            words.extend(self.checksum.as_deref().map(|c| ("the checksum", c)));
            words.extend(self.redirects.iter().map(|r| ("a redirect", r.as_str())));
            words.extend(self.s3.endpoint.as_deref().map(|e| ("the S3 endpoint", e)));
            words.extend(self.s3.region.as_deref().map(|r| ("the S3 region", r)));
            words.extend(self.s3.version_id.as_deref().map(|v| ("the S3 version", v)));
            words.extend(self.platform.as_deref().map(|p| ("the platform", p)));
            for (what, word) in words {
                if let Some(why) = unplain(word) {
                    return Err(refuse(what, format!("{word:?}: {why}")));
                }
            }
            if self.platform.as_deref().is_some_and(|p| p.contains(',')) {
                return Err(refuse(
                    "the platform",
                    "one platform alone: a component is one platform's build".into(),
                ));
            }
            Ok(())
        }

        /// The directive the build is of: one line of an Agentfile, its flags before its name
        /// as the parser takes them (D54).
        pub fn directive(&self) -> String {
            let mut line = vec![self.kind.directive().to_string()];
            line.extend(self.checksum.iter().map(|c| format!("--checksum={c}")));
            line.extend(self.redirects.iter().map(|r| format!("--redirect={r}")));
            line.extend(self.s3.endpoint.iter().map(|e| format!("--endpoint={e}")));
            line.extend(self.s3.region.iter().map(|r| format!("--region={r}")));
            if self.s3.path_style {
                line.push("--path-style".into());
            }
            line.extend(self.s3.version_id.iter().map(|v| format!("--version-id={v}")));
            if self.s3.aws_default_credentials {
                line.push("--credentials=aws-default".into());
            }
            line.extend([self.name.clone(), "FROM".into(), self.source.clone()]);
            line.join(" ")
        }

        /// Where the build lays it, as its directive does with no `TO` (§12.1).
        pub fn dir(&self) -> String {
            format!("{}/{}", self.kind.parent(), self.name)
        }
    }

    /// `request` as its frame.
    pub fn component_request(request: &ComponentRequest) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u8(VERSION);
        w.u8(request.kind.tag());
        w.text(&request.name)?;
        w.text(&request.source)?;
        optional(&mut w, &request.checksum)?;
        w.count(request.redirects.len())?;
        for r in &request.redirects {
            w.text(r)?;
        }
        optional(&mut w, &request.s3.endpoint)?;
        optional(&mut w, &request.s3.region)?;
        w.u8(u8::from(request.s3.path_style));
        optional(&mut w, &request.s3.version_id)?;
        w.u8(u8::from(request.s3.aws_default_credentials));
        optional(&mut w, &request.platform)?;
        w.text(&request.tag)?;
        write_access(&mut w, &request.secrets, &request.ssh)?;
        frame(to_host::COMPONENT, &w.done()?)
    }

    /// The request a [`to_host::COMPONENT`] frame's payload holds.
    pub fn read_component(payload: &[u8]) -> Result<ComponentRequest, RequestError> {
        let mut r = Reader(payload);
        let wire = RequestError::Wire;
        match r.u8().map_err(wire)? {
            VERSION => {}
            v => return Err(RequestError::Version(v)),
        }
        let kind = ComponentKind::of_tag(r.u8().map_err(wire)?).map_err(wire)?;
        let (name, source, checksum) = (
            r.text().map_err(wire)?,
            r.text().map_err(wire)?,
            read_optional(&mut r).map_err(wire)?,
        );
        let mut redirects = Vec::new();
        // Each holds at least its length.
        for _ in 0..r.count(4).map_err(wire)? {
            redirects.push(r.text().map_err(wire)?);
        }
        let s3 = S3Source {
            endpoint: read_optional(&mut r).map_err(wire)?,
            region: read_optional(&mut r).map_err(wire)?,
            path_style: r.flag().map_err(wire)?,
            version_id: read_optional(&mut r).map_err(wire)?,
            aws_default_credentials: r.flag().map_err(wire)?,
        };
        let platform = read_optional(&mut r).map_err(wire)?;
        let tag = r.text().map_err(wire)?;
        let (secrets, ssh) = read_access(&mut r).map_err(wire)?;
        r.end().map_err(wire)?;
        Ok(ComponentRequest {
            kind,
            name,
            source,
            checksum,
            redirects,
            s3,
            platform,
            tag,
            secrets,
            ssh,
        })
    }

    /// One of the host's answers.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Answer {
        Event(BuildEvent),
        Policy(Vec<String>),
        At(Vec<RangeInclusive<u32>>),
        Built(String),
        Failed(String),
    }

    impl Request {
        /// `spec` checked as a request, or refused, naming what of it: a context that is no
        /// directory or a file that is none, each by an absolute UTF-8 path; an empty platform
        /// or tag; an id said twice or empty; and what the build's command line could not say
        /// whole (an SSH id with `=`, a key's path with `,`: buildx's `--ssh` splits there).
        pub fn of(spec: &BuildSpec) -> Result<Request, BuildRefusal> {
            let context = absolute("the context", &spec.context)?;
            if !spec.context.is_dir() {
                return Err(refuse("the context", format!("{context}: not a directory")));
            }
            let file = match &spec.file {
                None => None,
                Some(file) => {
                    let path = absolute("the Agentfile", file)?;
                    if !file.is_file() {
                        return Err(refuse("the Agentfile", format!("{path}: not a file")));
                    }
                    Some(path)
                }
            };
            let named = |what: &'static str, value: &Option<String>| match value {
                Some(v) if v.trim().is_empty() || v.chars().any(char::is_whitespace) => {
                    Err(refuse(what, format!("{v:?}: empty or spaced")))
                }
                _ => Ok(value.clone()),
            };
            let platform = named("the platform", &spec.platform)?;
            let tag = named("the tag", &spec.tag)?;
            let (secrets, ssh) = access_of(&spec.secrets, &spec.ssh)?;
            Ok(Request {
                context,
                file,
                platform,
                tag,
                secrets,
                ssh,
            })
        }
    }

    fn refuse(what: &'static str, why: String) -> BuildRefusal {
        BuildRefusal::Spec { what, why }
    }

    /// `path` as the request carries it: absolute and UTF-8, else refused as `what`.
    fn absolute(what: &'static str, path: &Path) -> Result<String, BuildRefusal> {
        if !path.is_absolute() {
            return Err(refuse(what, format!("{}: not an absolute path", path.display())));
        }
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| refuse(what, format!("{}: not UTF-8", path.display())))
    }

    /// A build's secrets and SSH agents as a request carries them, or refused, naming what of
    /// them: an id said twice or empty, and what the build's command line could not say whole
    /// (an SSH id with `=`, a key's path with `,`: buildx's `--ssh` splits there).
    #[allow(clippy::type_complexity)]
    fn access_of(
        secrets: &[BuildSecret],
        agents: &[BuildSsh],
    ) -> Result<(Vec<(String, Source)>, Vec<(String, Vec<String>)>), BuildRefusal> {
        let mut ids = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for s in secrets {
            if s.id.is_empty() || !ids.insert(("secret", s.id.as_str())) {
                return Err(refuse(
                    "a secret's id",
                    format!("{:?}: empty or given twice", s.id),
                ));
            }
            let source = match &s.source {
                SecretSource::File(path) => Source::File(absolute("a secret's file", path)?),
                SecretSource::Env(name) if name.is_empty() || name.contains('=') => {
                    return Err(refuse(
                        "a secret's variable",
                        format!("{name:?}: no variable's name"),
                    ));
                }
                SecretSource::Env(name) => Source::Env(name.clone()),
            };
            out.push((s.id.clone(), source));
        }
        let mut ssh = Vec::new();
        for agent in agents {
            if agent.id.is_empty() || agent.id.contains('=') || !ids.insert(("ssh", agent.id.as_str())) {
                return Err(refuse(
                    "an SSH agent's id",
                    format!("{:?}: empty, with `=`, or given twice", agent.id),
                ));
            }
            let mut paths = Vec::new();
            for path in &agent.paths {
                let path = absolute("an SSH key's path", path)?;
                if path.contains(',') {
                    return Err(refuse(
                        "an SSH key's path",
                        format!("{path}: holds `,`, where --ssh splits"),
                    ));
                }
                paths.push(path);
            }
            ssh.push((agent.id.clone(), paths));
        }
        Ok((out, ssh))
    }

    fn write_access(
        w: &mut Writer,
        secrets: &[(String, Source)],
        ssh: &[(String, Vec<String>)],
    ) -> Result<(), WireError> {
        w.count(secrets.len())?;
        for (id, source) in secrets {
            w.text(id)?;
            match source {
                Source::File(path) => {
                    w.u8(0);
                    w.text(path)?;
                }
                Source::Env(name) => {
                    w.u8(1);
                    w.text(name)?;
                }
            }
        }
        w.count(ssh.len())?;
        for (id, paths) in ssh {
            w.text(id)?;
            w.count(paths.len())?;
            for path in paths {
                w.text(path)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn read_access(
        r: &mut Reader<'_>,
    ) -> Result<(Vec<(String, Source)>, Vec<(String, Vec<String>)>), WireError> {
        // Each secret holds at least its id's and value's lengths and its tag.
        let mut secrets = Vec::new();
        for _ in 0..r.count(9)? {
            let id = r.text()?;
            let source = match r.u8()? {
                0 => Source::File(r.text()?),
                1 => Source::Env(r.text()?),
                tag => {
                    return Err(WireError::Tag {
                        what: "a secret's source",
                        tag,
                    });
                }
            };
            secrets.push((id, source));
        }
        let mut ssh = Vec::new();
        for _ in 0..r.count(8)? {
            let id = r.text()?;
            let mut paths = Vec::new();
            for _ in 0..r.count(4)? {
                paths.push(r.text()?);
            }
            ssh.push((id, paths));
        }
        Ok((secrets, ssh))
    }

    fn optional(w: &mut Writer, value: &Option<String>) -> Result<(), WireError> {
        match value {
            None => w.u8(0),
            Some(v) => {
                w.u8(1);
                w.text(v)?;
            }
        }
        Ok(())
    }

    fn read_optional(r: &mut Reader<'_>) -> Result<Option<String>, WireError> {
        Ok(if r.flag()? { Some(r.text()?) } else { None })
    }

    /// `request` as its frame.
    pub fn request(request: &Request) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u8(VERSION);
        w.text(&request.context)?;
        optional(&mut w, &request.file)?;
        optional(&mut w, &request.platform)?;
        optional(&mut w, &request.tag)?;
        write_access(&mut w, &request.secrets, &request.ssh)?;
        frame(to_host::REQUEST, &w.done()?)
    }

    /// Why a request was not read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RequestError {
        /// A version this side does not read.
        Version(u8),
        Wire(WireError),
    }

    impl std::fmt::Display for RequestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Version(v) => write!(f, "a build request of version {v}; this side reads {VERSION}"),
                Self::Wire(e) => write!(f, "a build request: {e}"),
            }
        }
    }

    /// The request a [`to_host::REQUEST`] frame's payload holds.
    pub fn read_request(payload: &[u8]) -> Result<Request, RequestError> {
        let mut r = Reader(payload);
        let wire = RequestError::Wire;
        match r.u8().map_err(wire)? {
            VERSION => {}
            v => return Err(RequestError::Version(v)),
        }
        let (context, file, platform, tag) = (
            r.text().map_err(wire)?,
            read_optional(&mut r).map_err(wire)?,
            read_optional(&mut r).map_err(wire)?,
            read_optional(&mut r).map_err(wire)?,
        );
        let (secrets, ssh) = read_access(&mut r).map_err(wire)?;
        r.end().map_err(wire)?;
        Ok(Request {
            context,
            file,
            platform,
            tag,
            secrets,
            ssh,
        })
    }

    /// `event` as `shards api-build` tells it.
    pub fn event(event: &BuildEvent) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        let kind = match event {
            BuildEvent::Step { index, name } => {
                w.u32(*index);
                w.text(name)?;
                to_client::STEP
            }
            BuildEvent::Line { index, text } => {
                w.u32(*index);
                w.text(text)?;
                to_client::LINE
            }
            BuildEvent::Ended { index, how } => {
                w.u32(*index);
                match how {
                    StepEnd::Done => w.u8(0),
                    StepEnd::Cached => w.u8(1),
                    StepEnd::Failed(error) => {
                        w.u8(2);
                        w.text(error)?;
                    }
                    StepEnd::Canceled => w.u8(3),
                }
                to_client::STEP_ENDED
            }
        };
        frame(kind, &w.done()?)
    }

    /// What a policy's decision said as it refused one of the build's sources.
    pub fn policy(said: &[String]) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.count(said.len())?;
        for m in said {
            w.text(m)?;
        }
        frame(to_client::POLICY, &w.done()?)
    }

    /// The lines of the build's Agentfile its error is at.
    pub fn at(lines: &[RangeInclusive<u32>]) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.count(lines.len())?;
        for l in lines {
            w.u32(*l.start());
            w.u32(*l.end());
        }
        frame(to_client::AT, &w.done()?)
    }

    /// The build's end: its image's ID, or its error.
    pub fn end(built: &Result<String, String>) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        let kind = match built {
            Ok(id) => {
                w.text(id)?;
                to_client::BUILT
            }
            Err(message) => {
                w.text(message)?;
                to_client::FAILED
            }
        };
        frame(kind, &w.done()?)
    }

    /// The answer a frame of `kind` holds.
    pub fn answer(kind: u8, payload: &[u8]) -> Result<Answer, BuildRefusal> {
        let mut r = Reader(payload);
        let wire = BuildRefusal::Wire;
        let answer = match kind {
            to_client::STEP => Answer::Event(BuildEvent::Step {
                index: r.u32().map_err(wire)?,
                name: r.text().map_err(wire)?,
            }),
            to_client::LINE => Answer::Event(BuildEvent::Line {
                index: r.u32().map_err(wire)?,
                text: r.text().map_err(wire)?,
            }),
            to_client::STEP_ENDED => {
                let index = r.u32().map_err(wire)?;
                let how = match r.u8().map_err(wire)? {
                    0 => StepEnd::Done,
                    1 => StepEnd::Cached,
                    2 => StepEnd::Failed(r.text().map_err(wire)?),
                    3 => StepEnd::Canceled,
                    tag => {
                        return Err(wire(WireError::Tag {
                            what: "a step's end",
                            tag,
                        }));
                    }
                };
                Answer::Event(BuildEvent::Ended { index, how })
            }
            to_client::BUILT => Answer::Built(r.text().map_err(wire)?),
            to_client::FAILED => Answer::Failed(r.text().map_err(wire)?),
            to_client::POLICY => {
                let mut said = Vec::new();
                // Each message holds at least its length.
                for _ in 0..r.count(4).map_err(wire)? {
                    said.push(r.text().map_err(wire)?);
                }
                Answer::Policy(said)
            }
            to_client::AT => {
                let mut lines = Vec::new();
                // Each range is its first line and its last, a u32 each.
                for _ in 0..r.count(8).map_err(wire)? {
                    let (first, last) = (r.u32().map_err(wire)?, r.u32().map_err(wire)?);
                    if last < first {
                        return Err(wire(WireError::Order {
                            what: "a range of lines",
                        }));
                    }
                    lines.push(first..=last);
                }
                Answer::At(lines)
            }
            tag => {
                return Err(wire(WireError::Tag {
                    what: "a build's answer",
                    tag,
                }));
            }
        };
        r.end().map_err(wire)?;
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::wire::{
        Answer, ComponentRequest, Request, Source, answer, component_request, end, event, read_component,
        read_request, request,
    };
    use super::*;

    fn payload(frame: &[u8]) -> (u8, Vec<u8>) {
        let (kind, len) =
            shards_abi::run::parse_header(frame[..shards_abi::run::HEADER].try_into().unwrap()).unwrap();
        assert_eq!(len as usize, frame.len() - shards_abi::run::HEADER);
        (kind, frame[shards_abi::run::HEADER..].to_vec())
    }

    fn context() -> (shards_testdir::TempDir, BuildSpec) {
        let dir = shards_testdir::TempDir::new("builds").unwrap();
        std::fs::write(dir.path().join("Agentfile"), "FROM scratch\n").unwrap();
        let spec = BuildSpec {
            context: dir.path().to_path_buf(),
            ..BuildSpec::default()
        };
        (dir, spec)
    }

    #[test]
    fn a_spec_is_refused_naming_what_of_it_before_anything_starts() {
        let (dir, spec) = context();
        let refused = |spec: &BuildSpec| match Request::of(spec) {
            Err(BuildRefusal::Spec { what, .. }) => what,
            other => panic!("{other:?}"),
        };
        let with = |f: &dyn Fn(&mut BuildSpec)| {
            let mut s = spec.clone();
            f(&mut s);
            s
        };
        assert!(Request::of(&spec).is_ok());
        assert_eq!(refused(&with(&|s| s.context = "relative".into())), "the context");
        assert_eq!(
            refused(&with(&|s| s.context = dir.path().join("Agentfile"))),
            "the context"
        );
        assert_eq!(
            refused(&with(&|s| s.file = Some(dir.path().join("none")))),
            "the Agentfile"
        );
        assert_eq!(
            refused(&with(&|s| s.file = Some("Agentfile".into()))),
            "the Agentfile"
        );
        assert_eq!(
            refused(&with(&|s| s.platform = Some(String::new()))),
            "the platform"
        );
        assert_eq!(refused(&with(&|s| s.tag = Some("a b".into()))), "the tag");
        let secret = |id: &str, source: SecretSource| BuildSecret {
            id: id.into(),
            source,
        };
        assert_eq!(
            refused(&with(
                &|s| s.secrets = vec![secret("", SecretSource::Env("T".into()))]
            )),
            "a secret's id"
        );
        assert_eq!(
            refused(&with(&|s| {
                s.secrets = vec![
                    secret("t", SecretSource::Env("T".into())),
                    secret("t", SecretSource::Env("U".into())),
                ]
            })),
            "a secret's id"
        );
        assert_eq!(
            refused(&with(
                &|s| s.secrets = vec![secret("t", SecretSource::Env("A=B".into()))]
            )),
            "a secret's variable"
        );
        assert_eq!(
            refused(&with(
                &|s| s.secrets = vec![secret("t", SecretSource::File("k".into()))]
            )),
            "a secret's file"
        );
        let agent = |id: &str, paths: &[&str]| BuildSsh {
            id: id.into(),
            paths: paths.iter().map(PathBuf::from).collect(),
        };
        assert_eq!(
            refused(&with(&|s| s.ssh = vec![agent("a=b", &[])])),
            "an SSH agent's id"
        );
        assert_eq!(
            refused(&with(
                &|s| s.ssh = vec![agent("default", &[]), agent("default", &[])]
            )),
            "an SSH agent's id"
        );
        assert_eq!(
            refused(&with(&|s| s.ssh = vec![agent("k", &["/a,b"])])),
            "an SSH key's path"
        );
        // A secret and an agent may share an id: each is its own kind's.
        assert!(
            Request::of(&with(&|s| {
                s.secrets = vec![secret("x", SecretSource::Env("T".into()))];
                s.ssh = vec![agent("x", &[])];
            }))
            .is_ok()
        );
    }

    #[test]
    fn a_request_reads_back_as_it_was_sent_and_another_version_is_refused() {
        let sent = Request {
            context: "/c".into(),
            file: Some("/c/Agentfile".into()),
            platform: None,
            tag: Some("t".into()),
            secrets: vec![
                ("a".into(), Source::File("/s".into())),
                ("b".into(), Source::Env("E".into())),
            ],
            ssh: vec![
                ("default".into(), vec![]),
                ("k".into(), vec!["/k1".into(), "/k2".into()]),
            ],
        };
        let (kind, bytes) = payload(&request(&sent).unwrap());
        assert_eq!(kind, wire::to_host::REQUEST);
        assert_eq!(read_request(&bytes).unwrap(), sent);
        let mut later = bytes.clone();
        later[0] = wire::VERSION + 1;
        assert_eq!(
            read_request(&later),
            Err(wire::RequestError::Version(wire::VERSION + 1))
        );
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            read_request(&trailing),
            Err(wire::RequestError::Wire(WireError::Trailing))
        );
        // A count past what its bytes could hold allocates nothing and is refused.
        let mut hostile = request(&Request {
            context: "/c".into(),
            ..Request::default()
        })
        .unwrap()[shards_abi::run::HEADER..]
            .to_vec();
        let at = hostile.len() - 8;
        hostile[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            read_request(&hostile),
            Err(wire::RequestError::Wire(WireError::Short))
        );
    }

    #[test]
    fn every_answer_reads_back_as_it_was_told() {
        let events = [
            BuildEvent::Step {
                index: 1,
                name: "[1/2] FROM scratch".into(),
            },
            BuildEvent::Line {
                index: 1,
                text: "done".into(),
            },
            BuildEvent::Ended {
                index: 1,
                how: StepEnd::Done,
            },
            BuildEvent::Ended {
                index: 2,
                how: StepEnd::Cached,
            },
            BuildEvent::Ended {
                index: 3,
                how: StepEnd::Failed("exit code: 1".into()),
            },
            BuildEvent::Ended {
                index: 4,
                how: StepEnd::Canceled,
            },
        ];
        for e in events {
            let (kind, bytes) = payload(&event(&e).unwrap());
            assert_eq!(answer(kind, &bytes).unwrap(), Answer::Event(e));
        }
        let (kind, bytes) = payload(&end(&Ok("sha256:ab".into())).unwrap());
        assert_eq!(answer(kind, &bytes).unwrap(), Answer::Built("sha256:ab".into()));
        let (kind, bytes) = payload(&end(&Err("no".into())).unwrap());
        assert_eq!(answer(kind, &bytes).unwrap(), Answer::Failed("no".into()));
        let said = vec![
            "bucket agents is not trusted".to_string(),
            "and another".to_string(),
        ];
        let (kind, bytes) = payload(&wire::policy(&said).unwrap());
        assert_eq!(answer(kind, &bytes).unwrap(), Answer::Policy(said));
        let lines = vec![2..=2, 4..=6];
        let (kind, bytes) = payload(&wire::at(&lines).unwrap());
        assert_eq!(answer(kind, &bytes).unwrap(), Answer::At(lines));
        // A range whose last line comes before its first is refused; so is a count past what
        // its bytes could hold, before anything is allocated for it.
        assert_eq!(
            answer(wire::to_client::AT, &[0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0, 4]),
            Err(BuildRefusal::Wire(WireError::Order {
                what: "a range of lines"
            }))
        );
        for kind in [wire::to_client::AT, wire::to_client::POLICY] {
            assert_eq!(
                answer(kind, &u32::MAX.to_be_bytes()),
                Err(BuildRefusal::Wire(WireError::Short))
            );
        }
        assert!(matches!(
            answer(9, &[]),
            Err(BuildRefusal::Wire(WireError::Tag { tag: 9, .. }))
        ));
        assert!(matches!(
            answer(wire::to_client::STEP_ENDED, &[0, 0, 0, 1, 7]),
            Err(BuildRefusal::Wire(WireError::Tag { tag: 7, .. }))
        ));
    }

    /// `script` as an executable stand-in for `shards`, in `dir`, written by a process of its
    /// own (`shards_testdir::program`): one this process wrote itself failed its exec ETXTBSY
    /// where another test's child held it (CI 44379986478, aarch64 gnu, `Text file busy`).
    #[cfg(unix)]
    fn stand_in(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("shards");
        shards_testdir::program(&path, &format!("#!/bin/sh\n{script}")).unwrap();
        path
    }

    /// printf(1) of `frames`, the stand-in's answers, after it has read its request whole
    /// into `request` and its `SHARDS_HOME` into `home` in its own directory.
    #[cfg(unix)]
    fn answering(frames: &[Vec<u8>], then: &str) -> String {
        let octal: String = frames.concat().iter().map(|b| format!("\\{b:03o}")).collect();
        format!(
            "[ \"$1\" = api-build ] || exit 9\nd=$(dirname \"$0\")\ncat > \"$d/request\"\nprintf %s \"$SHARDS_HOME\" > \"$d/home\"\nprintf '{octal}'\n{then}\n"
        )
    }

    #[cfg(unix)]
    #[test]
    fn a_build_tells_its_steps_and_gives_back_its_image() {
        let (dir, spec) = context();
        let step = BuildEvent::Step {
            index: 1,
            name: "[1/1] FROM scratch".into(),
        };
        let done = BuildEvent::Ended {
            index: 1,
            how: StepEnd::Done,
        };
        let frames = [
            event(&step).unwrap(),
            event(&done).unwrap(),
            end(&Ok("sha256:0123".into())).unwrap(),
        ];
        let builds = Builds {
            binary: Some(stand_in(dir.path(), &answering(&frames, ""))),
            home: Some(dir.path().join("home")),
            env: Vec::new(),
        };
        let mut seen = Vec::new();
        let built = builds.build(&spec, &mut |e| seen.push(e));
        assert_eq!(
            built,
            Ok(Pinned {
                digest: "sha256:0123".into()
            })
        );
        assert_eq!(seen, [step, done]);
        // Its request went whole, and its home is the one asked for.
        let sent = std::fs::read(dir.path().join("request")).unwrap();
        assert_eq!(sent, request(&Request::of(&spec).unwrap()).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("home")).unwrap(),
            dir.path().join("home").to_str().unwrap()
        );
    }

    /// A builder's stand-in answering `frames`, in `dir`.
    #[cfg(unix)]
    fn answered_by(dir: &Path, frames: &[Vec<u8>]) -> Builds {
        Builds {
            binary: Some(stand_in(dir, &answering(frames, ""))),
            ..Builds::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_step_is_refused_by_its_number_and_name() {
        let (dir, spec) = context();
        let frames = [
            event(&BuildEvent::Step {
                index: 2,
                name: "[2/2] RUN false".into(),
            })
            .unwrap(),
            event(&BuildEvent::Ended {
                index: 2,
                how: StepEnd::Failed("exit code: 1".into()),
            })
            .unwrap(),
            wire::at(&[RangeInclusive::new(2, 2)]).unwrap(),
            end(&Err(
                "process \"/bin/sh -c false\" did not complete successfully: exit code: 1".into(),
            ))
            .unwrap(),
        ];
        assert_eq!(
            answered_by(dir.path(), &frames).build(&spec, &mut |_| {}),
            Err(BuildRefusal::Step {
                index: 2,
                name: "[2/2] RUN false".into(),
                error: "exit code: 1".into(),
                message: "process \"/bin/sh -c false\" did not complete successfully: exit code: 1".into(),
                lines: vec![RangeInclusive::new(2, 2)],
            })
        );
    }

    /// An Agentfile the build cannot read is refused at its line, typed beside the build's own
    /// words (P4.4): a caller marks the line without reading it out of the message.
    #[cfg(unix)]
    #[test]
    fn a_syntax_error_is_refused_at_its_line() {
        let (dir, spec) = context();
        let message =
            "failed to build: failed to solve: dockerfile parse error on line 2: unknown instruction: FOO";
        let frames = [
            wire::at(&[RangeInclusive::new(2, 2)]).unwrap(),
            end(&Err(message.into())).unwrap(),
        ];
        assert_eq!(
            answered_by(dir.path(), &frames).build(&spec, &mut |_| {}),
            Err(BuildRefusal::Failed {
                message: message.into(),
                lines: vec![RangeInclusive::new(2, 2)],
            })
        );
    }

    /// A build a policy refused is refused as the policy's, in its words, whatever step of the
    /// build's that failed (P4.4, D101); said as buildx says it.
    #[cfg(unix)]
    #[test]
    fn a_build_a_policy_refused_is_refused_in_the_policys_words() {
        let (dir, spec) = context();
        let message = "failed to build: failed to solve: failed to read dockerfile: failed to load LLB: error evaluating the source policy: source \"local://dockerfile\" not allowed by policy: action DENY";
        let frames = [
            event(&BuildEvent::Step {
                index: 1,
                name: "loading policies".into(),
            })
            .unwrap(),
            event(&BuildEvent::Ended {
                index: 1,
                how: StepEnd::Failed(message.into()),
            })
            .unwrap(),
            wire::policy(&["no source is trusted here".into()]).unwrap(),
            end(&Err(message.into())).unwrap(),
        ];
        let refused = answered_by(dir.path(), &frames).build(&spec, &mut |_| {});
        assert_eq!(
            refused,
            Err(BuildRefusal::Policy {
                said: vec!["no source is trusted here".into()],
                lines: Vec::new(),
                message: message.into(),
            })
        );
        assert_eq!(
            refused.unwrap_err().to_string(),
            format!("Policy: no source is trusted here\n{message}")
        );
    }

    /// A build's own variables reach its process over this one's, and its home over any of them.
    #[cfg(unix)]
    #[test]
    fn a_builds_variables_reach_its_host_and_its_home_comes_last() {
        let (dir, spec) = context();
        let frames = [end(&Ok("sha256:0123".into())).unwrap()];
        let builds = Builds {
            binary: Some(stand_in(
                dir.path(),
                &answering(
                    &frames,
                    "printf %s \"$SHARDS_TEST_TOKEN\" > \"$(dirname \"$0\")/token\"",
                ),
            )),
            home: Some(dir.path().join("home")),
            env: vec![
                ("SHARDS_TEST_TOKEN".into(), "t0k".into()),
                ("SHARDS_HOME".into(), "/elsewhere".into()),
            ],
        };
        assert!(builds.build(&spec, &mut |_| {}).is_ok());
        assert_eq!(std::fs::read_to_string(dir.path().join("token")).unwrap(), "t0k");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("home")).unwrap(),
            dir.path().join("home").to_str().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_build_that_fails_outside_a_step_or_ends_unsaid_is_refused_so() {
        let (dir, spec) = context();
        let failed = [end(&Err("failed to export image".into())).unwrap()];
        assert_eq!(
            answered_by(dir.path(), &failed).build(&spec, &mut |_| {}),
            Err(BuildRefusal::Failed {
                message: "failed to export image".into(),
                lines: Vec::new(),
            })
        );
        let builds = Builds {
            binary: Some(stand_in(
                dir.path(),
                &answering(&[], "echo 'shards: killed' >&2; exit 137"),
            )),
            ..Builds::default()
        };
        assert_eq!(
            builds.build(&spec, &mut |_| {}),
            Err(BuildRefusal::Ended {
                status: Some(137),
                said: "shards: killed\n".into()
            })
        );
    }

    #[test]
    fn a_missing_shards_is_refused_as_not_started() {
        let (dir, spec) = context();
        let builds = Builds {
            binary: Some(dir.path().join("no-shards-here")),
            ..Builds::default()
        };
        assert!(matches!(
            builds.build(&spec, &mut |_| {}),
            Err(BuildRefusal::Start(_))
        ));
    }

    fn harness(source: &str) -> ComponentSpec {
        ComponentSpec {
            kind: ComponentKind::Harness,
            name: "codex".into(),
            source: source.into(),
            checksum: None,
            redirects: Vec::new(),
            s3: S3Source::default(),
            platform: None,
            tag: "kiln/harness-codex:0.58".into(),
            secrets: Vec::new(),
            ssh: Vec::new(),
        }
    }

    #[test]
    fn a_component_is_its_directives_one_line() {
        let mut spec = harness("https://example.com/codex.tar.gz");
        spec.checksum = Some(format!("sha256:{}", "a".repeat(64)));
        spec.redirects = vec!["objects.example.com".into(), "cdn.example.com:8443".into()];
        let request = ComponentRequest::of(&spec).unwrap();
        assert_eq!(
            request.directive(),
            format!(
                "HARNESS --checksum=sha256:{} --redirect=objects.example.com --redirect=cdn.example.com:8443 codex FROM https://example.com/codex.tar.gz",
                "a".repeat(64)
            )
        );
        assert_eq!(request.dir(), "/harness/codex");
        let mut s3 = harness("s3://bucket/agents/a.tar");
        s3.kind = ComponentKind::Agent;
        s3.name = "worker".into();
        s3.s3 = S3Source {
            endpoint: Some("https://r2.example.com".into()),
            region: Some("auto".into()),
            path_style: true,
            version_id: Some("v1".into()),
            aws_default_credentials: true,
        };
        let request = ComponentRequest::of(&s3).unwrap();
        assert_eq!(
            request.directive(),
            "AGENT --endpoint=https://r2.example.com --region=auto --path-style --version-id=v1 --credentials=aws-default worker FROM s3://bucket/agents/a.tar"
        );
        assert_eq!(request.dir(), "/agents/worker");
        let mut mcp = harness("https://example.com/org/tools.git#v1.2.0");
        mcp.kind = ComponentKind::Mcp;
        mcp.name = "tools".into();
        let request = ComponentRequest::of(&mcp).unwrap();
        assert_eq!(
            request.directive(),
            "MCP tools FROM https://example.com/org/tools.git#v1.2.0"
        );
        assert_eq!(request.dir(), "/mcp/tools");
    }

    #[test]
    fn a_component_whose_words_would_be_more_than_its_directive_is_refused() {
        let refused = |spec: &ComponentSpec| match ComponentRequest::of(spec) {
            Err(BuildRefusal::Spec { what, .. }) => what,
            other => panic!("{other:?}"),
        };
        let with = |f: &dyn Fn(&mut ComponentSpec)| {
            let mut s = harness("https://example.com/codex.tar.gz");
            f(&mut s);
            s
        };
        // A newline would add an instruction to the Agentfile; a space, a word; `\`, the next
        // line; a quote, a group; `$`, a variable's value.
        for source in [
            "https://example.com/a.tar\nRUN rm -rf /",
            "https://example.com/a.tar\n",
            "https://example.com/a tar",
            "https://example.com/a.tar\\",
            "https://example.com/\"a\"",
            "https://example.com/$HOME",
            "",
        ] {
            assert_eq!(
                refused(&with(&|s| s.source = source.into())),
                "the source",
                "{source:?}"
            );
        }
        assert_eq!(refused(&with(&|s| s.name = "Codex".into())), "the name");
        assert_eq!(refused(&with(&|s| s.name = "1codex".into())), "the name");
        assert_eq!(refused(&with(&|s| s.name = "co dex".into())), "the name");
        assert_eq!(
            refused(&with(&|s| s.checksum = Some("sha256:a b".into()))),
            "the checksum"
        );
        assert_eq!(
            refused(&with(&|s| s.redirects = vec!["a.example.com\tb".into()])),
            "a redirect"
        );
        assert_eq!(
            refused(&with(&|s| s.s3.endpoint = Some("https://x\ny".into()))),
            "the S3 endpoint"
        );
        assert_eq!(refused(&with(&|s| s.tag = String::new())), "the tag");
        assert_eq!(
            refused(&with(&|s| s.platform = Some("linux/arm64,linux/amd64".into()))),
            "the platform"
        );
        assert_eq!(
            refused(&with(&|s| s.secrets = vec![BuildSecret {
                id: String::new(),
                source: SecretSource::Env("T".into())
            }])),
            "a secret's id"
        );
        assert!(ComponentRequest::of(&with(&|_| {})).is_ok());
    }

    #[test]
    fn a_component_request_reads_back_as_it_was_sent() {
        let mut spec = harness("s3://bucket/codex.tar");
        spec.checksum = Some(format!("sha256:{}", "b".repeat(64)));
        spec.redirects = vec!["r.example.com".into()];
        spec.s3 = S3Source {
            endpoint: Some("https://s3.example.com".into()),
            region: Some("eu-west-1".into()),
            path_style: true,
            version_id: None,
            aws_default_credentials: false,
        };
        spec.platform = Some("linux/amd64".into());
        spec.secrets = vec![BuildSecret {
            id: "AWS_ACCESS_KEY_ID".into(),
            source: SecretSource::Env("KEY".into()),
        }];
        spec.ssh = vec![BuildSsh {
            id: "default".into(),
            paths: vec![],
        }];
        let sent = ComponentRequest::of(&spec).unwrap();
        let (kind, bytes) = payload(&component_request(&sent).unwrap());
        assert_eq!(kind, wire::to_host::COMPONENT);
        assert_eq!(read_component(&bytes).unwrap(), sent);
        let mut later = bytes.clone();
        later[0] = wire::VERSION + 1;
        assert_eq!(
            read_component(&later),
            Err(wire::RequestError::Version(wire::VERSION + 1))
        );
        let mut kind_of = bytes.clone();
        kind_of[1] = 9;
        assert_eq!(
            read_component(&kind_of),
            Err(wire::RequestError::Wire(WireError::Tag {
                what: "a component's kind",
                tag: 9
            }))
        );
        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            read_component(&trailing),
            Err(wire::RequestError::Wire(WireError::Trailing))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_component_is_built_by_its_own_request_and_gives_back_its_artifact() {
        let dir = shards_testdir::TempDir::new("components").unwrap();
        let frames = [end(&Ok("sha256:c0ffee".into())).unwrap()];
        let builds = Builds {
            binary: Some(stand_in(dir.path(), &answering(&frames, ""))),
            home: Some(dir.path().join("home")),
            env: Vec::new(),
        };
        let mut spec = harness("https://example.com/codex.tar.gz");
        spec.checksum = Some(format!("sha256:{}", "c".repeat(64)));
        assert_eq!(
            builds.component(&spec, &mut |_| {}),
            Ok(Pinned {
                digest: "sha256:c0ffee".into()
            })
        );
        let sent = std::fs::read(dir.path().join("request")).unwrap();
        assert_eq!(
            sent,
            component_request(&ComponentRequest::of(&spec).unwrap()).unwrap()
        );
        // A spec refused here starts nothing.
        std::fs::remove_file(dir.path().join("request")).unwrap();
        spec.source = "https://example.com/a b".into();
        assert!(matches!(
            builds.component(&spec, &mut |_| {}),
            Err(BuildRefusal::Spec {
                what: "the source",
                ..
            })
        ));
        assert!(!dir.path().join("request").exists());
    }
}
