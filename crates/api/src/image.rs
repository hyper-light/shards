//! An image booted for an orchestrator (docs/design/architecture.md D231): a Shards image
//! built from an Agentfile, run under shards-init with no daemon, each member started in
//! its confined domain as the Agentfile declares it, and handed to the orchestrator: each
//! member's output and end, what it stages into each member at create, its published ports,
//! the image's grants as it narrows them, host directories, stats, and stop.
//!
//! [`ImageShards::boot`] starts `shards api-run`, one process of the `shards` binary for
//! the run, its host, and speaks [`wire`] with it over that process's stdin and stdout
//! (credentials there alone, never on its command line); its stderr is diagnostics, its last
//! bytes kept for a refusal to quote. The run ends with its [`ImageGuest`]: dropped,
//! stopped, or its channel gone, the VM goes, so no microVM outlives its owner.
//!
//! What a member is given is native-sign-in delegation, not an opaque credential
//! (managed-runtime-plan.md G18): it reads every byte of it.
//!
//! An image declaring `NESTING` gives the run's own command the nesting grant where the
//! spec asks for it ([`ImageSpec::nesting`], D234), with a deadline for its readiness
//! ([`ImageSpec::ready_by`]), which the command's `READY=1` moves and
//! [`ImageGuest::wait_ready`] tells.

use std::path::PathBuf;
use std::time::Duration;

mod session;
pub mod wire;

pub use wire::{
    Bound, Channel, Keep, MemberInfo, MemberKind, Narrowing, Proto, Publish, Ready, Receiving, RunEnd,
    Secret, Stats, StatsScope,
};

/// A member, as the image's Agentfile names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberName {
    pub kind: MemberKind,
    pub name: String,
}

impl MemberName {
    pub fn harness(name: &str) -> MemberName {
        MemberName {
            kind: MemberKind::Harness,
            name: name.into(),
        }
    }

    pub fn agent(name: &str) -> MemberName {
        MemberName {
            kind: MemberKind::Agent,
            name: name.into(),
        }
    }
}

/// A file staged under a member's home: its own scratch tmpfs, where its `HOME` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFile {
    /// Relative to the member's home: no `..`, no empty or `.` component.
    pub path: String,
    /// Permission bits alone: at most 0o777.
    pub mode: u32,
    pub bytes: Secret,
}

/// What the orchestrator gives one member at create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSpec {
    pub member: MemberName,
    /// Variables over its image's, each `(KEY, VALUE)`; `SHARDS_DOMAIN` is init's.
    pub env: Vec<(String, Secret)>,
    pub files: Vec<MemberFile>,
    /// Its stdin, a pipe the orchestrator writes ([`ImageGuest::take_input`]); else
    /// `/dev/null`, as every member's is today.
    pub stdin: bool,
    /// A terminal for its stdio.
    pub terminal: bool,
}

/// A host directory the guest sees at `at`, as `-v HOST:AT[:ro]` gives one: a member gets
/// it where the image's `VOLUME` at `at` is for it (D109), the run's own command where no
/// `VOLUME` declares `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageShare {
    pub host: PathBuf,
    pub at: String,
    pub read_only: bool,
}

/// An image to boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSpec {
    /// As `shards run` names one in its home's store (`SHARDS_HOME`, else the default).
    pub image: String,
    /// Its vCPUs; 0 as `shards run` sizes it.
    pub vcpus: u32,
    /// Its memory in MiB; 0 as `shards run` sizes it.
    pub memory_mib: u64,
    /// The run's own command; none for the image's members alone.
    pub command: Option<Vec<String>>,
    pub members: Vec<MemberSpec>,
    pub publish: Vec<Publish>,
    /// The image's grants past the microVM, kept or dropped, never added to.
    pub egress: Narrowing,
    pub shares: Vec<ImageShare>,
    /// The nesting grant for the run's own command (D234): the run's consent, as
    /// `--security-opt nesting=true` gives it to `run vm`. Given only where the image's
    /// Agentfile declares `NESTING`, and so no agent or harness: asked of an image that does
    /// not declare it, refused as [`ImageRefusal::Widening`]; of one crafted with it beside
    /// an agent or harness, as [`ImageRefusal::Image`].
    pub nesting: bool,
    /// The grant's deadline (D234): how long the run's own command has to say it is ready,
    /// from when the run's host reads the request, which makes it an instant on the host's
    /// clock; none for none. The grant's alone: without [`nesting`](Self::nesting), refused.
    /// A deadline missed is told ([`Ready::Passed`]), never enforced: the run goes on.
    pub ready_by: Option<Duration>,
    /// The `shards` binary: else `SHARDS_BIN`, else `shards` on `PATH`.
    pub binary: Option<PathBuf>,
    /// Variables of the run host's own process, over this process's: `SHARDS_HOME` (whose
    /// store holds the image), `SHARDS_KERNEL` and `SHARDS_INIT` (a guest of the caller's,
    /// as `shards run` takes them). Never a member's: those are [`MemberSpec::env`], which
    /// travel on the channel alone.
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

/// How a member ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberEnd {
    /// Its population ended, its leader with this status (as `docker run` reports one).
    Exited(u32),
    /// The run ended first, and the member with it.
    WithRun,
}

/// What can't be done, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRefusal {
    /// This host has no backend shards boots Linux with.
    NoBackend { os: &'static str, arch: &'static str },
    /// The spec, by the field named.
    Spec { what: &'static str, why: String },
    /// The image, its Agentfile, or a member it names.
    Image(String),
    /// A grant the image does not give.
    Widening(String),
    /// A port, a share, the network or the VM, as the run host said.
    Boot(String),
    /// What this does not do yet, by name.
    Unsupported { what: &'static str, why: String },
    /// The run has ended.
    Ended(RunEnd),
    /// The run host broke the channel's contract, or ended without its run's end.
    Host(String),
    /// What this process could not do: start the run host, a pipe, a thread.
    Os {
        what: &'static str,
        kind: std::io::ErrorKind,
    },
}

impl std::fmt::Display for ImageRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBackend { os, arch } => write!(f, "shards boots no guest on {os} {arch}"),
            Self::Spec { what, why } | Self::Unsupported { what, why } => write!(f, "{what}: {why}"),
            Self::Image(why) => write!(f, "the image: {why}"),
            Self::Widening(why) => write!(f, "past the image's grants: {why}"),
            Self::Boot(why) => write!(f, "the run did not start: {why}"),
            Self::Ended(end) => write!(f, "the run has ended: {end:?}"),
            Self::Host(why) => write!(f, "the run's host: {why}"),
            Self::Os { what, kind } => write!(f, "{what}: {kind:?}"),
        }
    }
}

impl std::error::Error for ImageRefusal {}

/// What boots images.
pub trait Images: Send + Sync {
    fn backend(&self) -> Result<crate::Backend, ImageRefusal>;
    /// Boots `spec`'s image, returning once its members have started, or why not, waiting
    /// at most `patience` for it (`Duration::MAX`: until the run starts or its host ends):
    /// past it, the run's host is killed and its run refused with what it last said. It is
    /// the caller's until it drops, which ends it.
    fn boot(&self, spec: &ImageSpec, patience: Duration) -> Result<Box<dyn ImageGuest>, ImageRefusal>;
}

/// A booted image. Dropping it ends it.
pub trait ImageGuest: Send + Sync {
    /// Every member the image declares, by its declared index.
    fn members(&self) -> &[MemberInfo];
    /// Each port published, as bound, and the members it reaches ([`Bound::to`]).
    fn published(&self) -> &[Bound];
    /// A member's channel, as a pipe's read end, its bytes exactly as the member wrote
    /// them, ending at the channel's end; once each. What no one reads backs up into that
    /// channel alone. Refused for a member that does not run.
    fn take_output(&self, member: &MemberName, channel: Channel)
    -> Result<std::io::PipeReader, ImageRefusal>;
    /// A member's stdin, asked for at create ([`MemberSpec::stdin`]), as a pipe's write end:
    /// its close ends the member's input.
    fn take_input(&self, member: &MemberName) -> Result<std::io::PipeWriter, ImageRefusal>;
    /// The member's end, waiting at most `timeout` (`Duration::MAX`: until it comes).
    fn wait_member(&self, member: &MemberName, timeout: Duration) -> Result<Option<MemberEnd>, ImageRefusal>;
    /// The use of the run, of its own command or of a member, as its init reads its cgroup,
    /// waiting at most `patience` (`Duration::MAX`: until it answers or the run ends).
    fn stats(&self, scope: StatsScope, patience: Duration) -> Result<Stats, ImageRefusal>;
    /// The readiness of the run's own command under the nesting grant (D234), as its init
    /// last said it, once it differs from `seen` (none: once there is any), waiting at most
    /// `timeout` (`Duration::MAX`: until it comes; `Duration::ZERO`: as it stands). Its
    /// deadline passed ([`Ready::Passed`]) moves on to ready late at its `READY=1`; ready is
    /// where it stays. Refused for a run whose spec does not ask for the grant, and once the
    /// run has ended with nothing newer ([`ImageRefusal::Ended`]).
    fn wait_ready(&self, seen: Option<Ready>, timeout: Duration) -> Result<Option<Ready>, ImageRefusal>;
    /// Ends it at once: its VM's process is killed, and [`wait_for`](Self::wait_for) then
    /// returns [`RunEnd::Stopped`]. Never refused, as a VM left running is a leak. Returns
    /// once the run's end is told, its VM gone; its host then reaps what it started and
    /// exits on its own.
    fn stop(&self) -> Result<(), ImageRefusal>;
    /// Its end, once it has come, waiting at most `timeout` (`Duration::MAX`: until it
    /// comes).
    fn wait_for(&self, timeout: Duration) -> Result<Option<RunEnd>, ImageRefusal>;
}

/// The images the `shards` binary boots.
#[derive(Debug, Default, Clone, Copy)]
pub struct ImageShards;

impl ImageShards {
    pub fn new() -> ImageShards {
        ImageShards
    }
}

impl Images for ImageShards {
    fn backend(&self) -> Result<crate::Backend, ImageRefusal> {
        crate::backend().map_err(no_backend)
    }

    fn boot(&self, spec: &ImageSpec, patience: Duration) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        crate::backend().map_err(no_backend)?;
        let request = request(spec)?;
        Ok(Box::new(session::Session::start(spec, &request, patience)?))
    }
}

fn no_backend(r: crate::Refusal) -> ImageRefusal {
    match r {
        crate::Refusal::NoBackend { os, arch } => ImageRefusal::NoBackend { os, arch },
        other => ImageRefusal::Boot(other.to_string()),
    }
}

/// `spec` as the run host reads it, refused here where it could never be run: a member's
/// terminal (owed, G24), a share's path that is no UTF-8 or is relative, a variable's key
/// that holds `=` or a NUL, or a key or file the guest would refuse.
fn request(spec: &ImageSpec) -> Result<wire::Request, ImageRefusal> {
    let mut members = Vec::with_capacity(spec.members.len());
    for m in &spec.members {
        if m.terminal {
            return Err(ImageRefusal::Unsupported {
                what: "a member's terminal",
                why: "a pty in a member's own namespaces is owed (managed-runtime-plan.md G24); \
                      its stdio are pipes"
                    .into(),
            });
        }
        if m.stdin {
            return Err(ImageRefusal::Unsupported {
                what: "a member's stdin",
                why: "a member's input connection is owed (D231's next slice); its stdin is /dev/null".into(),
            });
        }
        let mut env = Vec::with_capacity(m.env.len());
        for (key, value) in &m.env {
            let mut variable = Vec::with_capacity(key.len() + 1 + value.0.len());
            variable.extend_from_slice(key.as_bytes());
            variable.push(b'=');
            variable.extend_from_slice(&value.0);
            if key.contains('=') {
                return Err(ImageRefusal::Spec {
                    what: "a member's variable",
                    why: format!("{key:?}: a key holding '='"),
                });
            }
            shards_abi::run::members::variable_ok(&variable).map_err(|e| ImageRefusal::Spec {
                what: "a member's variable",
                why: format!("{key:?}: {e}"),
            })?;
            env.push(Secret(variable));
        }
        let mut files = Vec::with_capacity(m.files.len());
        for f in &m.files {
            shards_abi::run::members::file(0, f.mode, f.path.as_bytes(), &[]).map_err(|e| {
                ImageRefusal::Spec {
                    what: "a member's file",
                    why: format!("{}: {e}", f.path),
                }
            })?;
            files.push(wire::StagedFile {
                path: f.path.clone(),
                mode: f.mode,
                bytes: f.bytes.clone(),
            });
        }
        members.push(wire::MemberStage {
            kind: m.member.kind,
            name: m.member.name.clone(),
            env,
            files,
        });
    }
    let mut shares = Vec::with_capacity(spec.shares.len());
    for s in &spec.shares {
        let host = s
            .host
            .to_str()
            .filter(|_| s.host.is_absolute())
            .ok_or_else(|| ImageRefusal::Spec {
                what: "a share",
                why: format!(
                    "{}: a share's directory is an absolute UTF-8 path",
                    s.host.display()
                ),
            })?;
        if !s.at.starts_with('/') {
            return Err(ImageRefusal::Spec {
                what: "a share",
                why: format!("{:?}: where the guest sees it is an absolute path", s.at),
            });
        }
        shares.push(wire::ShareAt {
            host: host.into(),
            at: s.at.clone(),
            read_only: s.read_only,
        });
    }
    // The instant it becomes is nanoseconds since the Unix epoch in a u64, as `ready-by=`
    // carries it to init (D234): a budget past what one counts could never be one.
    let ready_by = spec
        .ready_by
        .map(|budget| {
            u64::try_from(budget.as_nanos()).map_err(|_| ImageRefusal::Spec {
                what: "the deadline",
                why: format!("{budget:?}: past the nanoseconds a deadline counts (a u64's)"),
            })
        })
        .transpose()?;
    let request = wire::Request {
        image: spec.image.clone(),
        vcpus: spec.vcpus,
        memory_mib: spec.memory_mib,
        command: spec.command.clone(),
        members,
        publish: spec.publish.clone(),
        egress: spec.egress.clone(),
        shares,
        nesting: spec.nesting,
        ready_by,
    };
    // Past the channel's bound it would never be read; the guest's bound is the same one.
    request.encode().map_err(|e| ImageRefusal::Spec {
        what: "the spec",
        why: e.to_string(),
    })?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ImageSpec {
        ImageSpec {
            image: "codex:local".into(),
            vcpus: 2,
            memory_mib: 1024,
            command: None,
            members: vec![MemberSpec {
                member: MemberName::harness("codex"),
                env: vec![("OPENAI_API_KEY".into(), Secret::from("sk-1"))],
                files: vec![MemberFile {
                    path: ".codex/auth.json".into(),
                    mode: 0o600,
                    bytes: Secret::from("{}"),
                }],
                stdin: false,
                terminal: false,
            }],
            publish: Vec::new(),
            egress: Narrowing::default(),
            shares: vec![ImageShare {
                host: std::env::current_dir().unwrap(),
                at: "/work".into(),
                read_only: true,
            }],
            nesting: false,
            ready_by: None,
            binary: None,
            env: Vec::new(),
        }
    }

    /// A spec becomes the run host's request, a variable `KEY=VALUE`.
    #[test]
    fn a_spec_is_the_request_the_host_reads() {
        let r = request(&spec()).unwrap();
        assert_eq!(r.members[0].env, vec![Secret::from("OPENAI_API_KEY=sk-1")]);
        assert_eq!(r.members[0].files[0].mode, 0o600);
        assert!(r.shares[0].read_only);
        assert_eq!((r.nesting, r.ready_by), (false, None));
        assert_eq!(wire::Request::decode(&r.encode().unwrap()).unwrap(), r);
    }

    /// The nesting grant's consent and its deadline (D234) reach the run host as asked:
    /// the deadline a budget in nanoseconds, its host's to make an instant; one past what
    /// an instant in nanoseconds counts is refused by field before anything starts. A
    /// deadline without the grant is the run host's to refuse, in the daemon's words.
    #[test]
    fn the_nesting_grant_and_its_deadline_are_asked_as_given() {
        for (nesting, budget, sent) in [
            (true, None, None),
            (true, Some(Duration::ZERO), Some(0)),
            (true, Some(Duration::new(5, 7)), Some(5_000_000_007)),
            (true, Some(Duration::from_nanos(u64::MAX)), Some(u64::MAX)),
            (false, Some(Duration::from_secs(1)), Some(1_000_000_000)),
        ] {
            let mut s = spec();
            s.nesting = nesting;
            s.ready_by = budget;
            let r = request(&s).unwrap();
            assert_eq!((r.nesting, r.ready_by), (nesting, sent), "{budget:?}");
        }
        let mut s = spec();
        s.nesting = true;
        for budget in [
            Duration::from_nanos(u64::MAX) + Duration::from_nanos(1),
            Duration::MAX,
        ] {
            s.ready_by = Some(budget);
            assert!(
                matches!(
                    request(&s),
                    Err(ImageRefusal::Spec {
                        what: "the deadline",
                        ..
                    })
                ),
                "{budget:?}"
            );
        }
    }

    /// What the guest would refuse is refused here, by field, before anything starts.
    #[test]
    fn what_the_guest_refuses_is_refused_first() {
        let refused = |s: ImageSpec| match request(&s) {
            Err(ImageRefusal::Spec { what, .. } | ImageRefusal::Unsupported { what, .. }) => what,
            other => panic!("{other:?}"),
        };
        let mut s = spec();
        s.members[0].env = vec![("SHARDS_DOMAIN".into(), Secret::from("agent x"))];
        assert_eq!(refused(s), "a member's variable");
        let mut s = spec();
        s.members[0].env = vec![("A=B".into(), Secret::from("c"))];
        assert_eq!(refused(s), "a member's variable");
        let mut s = spec();
        s.members[0].files[0].path = "../escape".into();
        assert_eq!(refused(s), "a member's file");
        let mut s = spec();
        s.members[0].files[0].mode = 0o4755;
        assert_eq!(refused(s), "a member's file");
        let mut s = spec();
        s.shares[0].host = PathBuf::from("relative");
        assert_eq!(refused(s), "a share");
        let mut s = spec();
        s.shares[0].at = "work".into();
        assert_eq!(refused(s), "a share");
        let mut s = spec();
        s.members[0].terminal = true;
        assert_eq!(refused(s), "a member's terminal");
        let mut s = spec();
        s.members[0].stdin = true;
        assert_eq!(refused(s), "a member's stdin");
    }

    /// A spec past the channel's bound is refused before the run host starts.
    #[test]
    fn a_spec_past_the_bound_is_refused() {
        let mut s = spec();
        s.members[0].files[0].bytes = Secret(vec![0; shards_abi::run::MAX_PAYLOAD as usize]);
        assert!(matches!(
            request(&s),
            Err(ImageRefusal::Spec { what: "the spec", .. })
        ));
    }
}
