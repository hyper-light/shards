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

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::command::{Command, NotStarted};
use crate::image::wire::{Reader, WireError, Writer, frame};

/// Where builds run: which `shards` (else `SHARDS_BIN`, else `shards` on `PATH`) and which
/// home's store they write to (else this process's `SHARDS_HOME`, else the default).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Builds {
    pub binary: Option<PathBuf>,
    pub home: Option<PathBuf>,
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
/// in its store whatever its tags are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub digest: String,
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

/// Why a build made no image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildRefusal {
    /// The spec, refused before anything started: which part, and why.
    Spec { what: &'static str, why: String },
    /// A step failed: its number and name, its own error, and the build's.
    Step {
        index: u32,
        name: String,
        error: String,
        message: String,
    },
    /// The build failed outside any step (its Agentfile's text, its plan, a base image's
    /// pull, its export), in shards' words.
    Failed(String),
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
            Self::Step { message, .. } | Self::Failed(message) => f.write_str(message),
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
        let env: Vec<(OsString, OsString)> = self
            .home
            .iter()
            .map(|home| (OsString::from("SHARDS_HOME"), home.clone().into_os_string()))
            .collect();
        let mut command = Command::start(self.binary.as_deref(), &[OsStr::new("api-build")], &env)?;
        let asked = wire::request(&request)
            .map_err(BuildRefusal::Wire)
            .and_then(|frame| {
                let os = |e: std::io::Error| BuildRefusal::Os {
                    what: "the build's request",
                    kind: e.kind(),
                };
                let input = command.input().ok_or(BuildRefusal::Os {
                    what: "the build's request",
                    kind: std::io::ErrorKind::BrokenPipe,
                })?;
                input.write_all(&frame).map_err(os)
            });
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

/// The steps a build's events named, and the first that failed: what its refusal says.
#[derive(Debug, Default)]
struct Steps {
    names: BTreeMap<u32, String>,
    failed: Option<(u32, String)>,
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

    fn refusal(&mut self, message: String) -> BuildRefusal {
        match self.failed.take() {
            Some((index, error)) => BuildRefusal::Step {
                index,
                name: self.names.get(&index).cloned().unwrap_or_default(),
                error,
                message,
            },
            None => BuildRefusal::Failed(message),
        }
    }
}

/// The channel between [`Builds::build`] and `shards api-build`: frames on its stdin (the
/// request, [`REQUEST`](to_host::REQUEST)) and its stdout (events, then its end), with the run
/// protocol's header, as an image run's channel (`crate::image::wire`) frames them.
pub mod wire {
    use super::*;

    /// The request's version, its first byte: changed only by appending.
    pub const VERSION: u8 = 1;

    /// What the client sends.
    pub mod to_host {
        /// The build to make ([`super::Request`]), the one frame the client sends.
        pub const REQUEST: u8 = 1;
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

    /// One of the host's answers.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Answer {
        Event(BuildEvent),
        Built(String),
        Failed(String),
    }

    impl Request {
        /// `spec` checked as a request, or refused, naming what of it: a context that is no
        /// directory or a file that is none, each by an absolute UTF-8 path; an empty platform
        /// or tag; an id said twice or empty; and what the build's command line could not say
        /// whole (an SSH id with `=`, a key's path with `,`: buildx's `--ssh` splits there).
        pub fn of(spec: &BuildSpec) -> Result<Request, BuildRefusal> {
            let refuse = |what: &'static str, why: String| BuildRefusal::Spec { what, why };
            let utf8 = |what: &'static str, path: &Path| {
                path.to_str()
                    .map(str::to_string)
                    .ok_or_else(|| refuse(what, format!("{}: not UTF-8", path.display())))
            };
            let absolute = |what: &'static str, path: &Path| {
                if path.is_absolute() {
                    utf8(what, path)
                } else {
                    Err(refuse(what, format!("{}: not an absolute path", path.display())))
                }
            };
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
            let mut ids = std::collections::BTreeSet::new();
            let mut secrets = Vec::new();
            for s in &spec.secrets {
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
                secrets.push((s.id.clone(), source));
            }
            let mut ssh = Vec::new();
            for agent in &spec.ssh {
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
        w.count(request.secrets.len())?;
        for (id, source) in &request.secrets {
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
        w.count(request.ssh.len())?;
        for (id, paths) in &request.ssh {
            w.text(id)?;
            w.count(paths.len())?;
            for path in paths {
                w.text(path)?;
            }
        }
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
        // Each secret holds at least its id's and value's lengths and its tag.
        let mut secrets = Vec::new();
        for _ in 0..r.count(9).map_err(wire)? {
            let id = r.text().map_err(wire)?;
            let source = match r.u8().map_err(wire)? {
                0 => Source::File(r.text().map_err(wire)?),
                1 => Source::Env(r.text().map_err(wire)?),
                tag => {
                    return Err(wire(WireError::Tag {
                        what: "a secret's source",
                        tag,
                    }));
                }
            };
            secrets.push((id, source));
        }
        let mut ssh = Vec::new();
        for _ in 0..r.count(8).map_err(wire)? {
            let id = r.text().map_err(wire)?;
            let mut paths = Vec::new();
            for _ in 0..r.count(4).map_err(wire)? {
                paths.push(r.text().map_err(wire)?);
            }
            ssh.push((id, paths));
        }
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
    use super::wire::{Answer, Request, Source, answer, end, event, read_request, request};
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
        assert!(matches!(
            answer(9, &[]),
            Err(BuildRefusal::Wire(WireError::Tag { tag: 9, .. }))
        ));
        assert!(matches!(
            answer(wire::to_client::STEP_ENDED, &[0, 0, 0, 1, 7]),
            Err(BuildRefusal::Wire(WireError::Tag { tag: 7, .. }))
        ));
    }

    /// `script` as an executable stand-in for `shards`, in `dir`.
    #[cfg(unix)]
    fn stand_in(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("shards");
        std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
            end(&Err(
                "process \"/bin/sh -c false\" did not complete successfully: exit code: 1".into(),
            ))
            .unwrap(),
        ];
        let builds = Builds {
            binary: Some(stand_in(dir.path(), &answering(&frames, ""))),
            home: None,
        };
        assert_eq!(
            builds.build(&spec, &mut |_| {}),
            Err(BuildRefusal::Step {
                index: 2,
                name: "[2/2] RUN false".into(),
                error: "exit code: 1".into(),
                message: "process \"/bin/sh -c false\" did not complete successfully: exit code: 1".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_build_that_fails_outside_a_step_or_ends_unsaid_is_refused_so() {
        let (dir, spec) = context();
        let failed = [end(&Err("dockerfile parse error on line 1".into())).unwrap()];
        let builds = Builds {
            binary: Some(stand_in(dir.path(), &answering(&failed, ""))),
            home: None,
        };
        assert_eq!(
            builds.build(&spec, &mut |_| {}),
            Err(BuildRefusal::Failed("dockerfile parse error on line 1".into()))
        );
        let builds = Builds {
            binary: Some(stand_in(
                dir.path(),
                &answering(&[], "echo 'shards: killed' >&2; exit 137"),
            )),
            home: None,
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
            home: None,
        };
        assert!(matches!(
            builds.build(&spec, &mut |_| {}),
            Err(BuildRefusal::Start(_))
        ));
    }
}
