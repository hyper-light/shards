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
//!
//! A run's credential grants name credentials by reference ([`ImageSpec::credentials`],
//! D242); their values are leased just in time by the orchestrator's [`Lessor`] for the run,
//! as the run's host asks, and every lease it gave is released as the run ends.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod session;
pub mod wire;

pub use wire::{
    Bound, Channel, CredentialGrant, Fill, GuestTimes, HostUse, Keep, LeaseRefusal, MemberInfo, MemberKind,
    MemberReach, Microvm, Narrowing, PlugRefusal, Proto, Publish, Ready, Receiving, RunEnd, Secret, Stats,
    StatsScope, VcpuTime, decide,
};

/// A credential's lease, as a run's [`Lessor`] gives it (D242): what its release names it
/// by, the credential's value, and when the lease ends, on this process's clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leased {
    /// The lessor's name for the lease, which [`Lessor::release`] is given: never its value.
    pub lease: String,
    pub value: Secret,
    pub until: Instant,
}

/// What answers a run's credential leases (D242): the orchestrator's, for that run alone,
/// from its broker. The run's injector asks for a credential's value just in time, by
/// reference, through the run's host; the run's channel names the run, which the lessor binds
/// its leases to toward its broker. Every lease it gives is released as the run ends, by
/// whatever end.
pub trait Lessor: Send + Sync {
    /// Credential `reference`'s value, leased to the run now, or why not, typed. Called on a
    /// thread of its own, once for each ask, `reference` one of the run's grants.
    fn lease(&self, reference: &str) -> Result<Leased, LeaseRefusal>;
    /// The lease [`lease`](Self::lease) named `lease`, released: the run has ended, or the
    /// lease was given after it did or ended before it could be passed on. Called once for
    /// each lease given.
    fn release(&self, lease: &str);
}

/// A run's [`Lessor`], as its spec holds it: equal to another where it is the same one, and
/// shown by name alone.
#[derive(Clone)]
pub struct RunLessor(pub Arc<dyn Lessor>);

impl std::fmt::Debug for RunLessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunLessor")
    }
}

impl PartialEq for RunLessor {
    fn eq(&self, other: &RunLessor) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for RunLessor {}

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
    /// Its working directory, absolute and plain: the project path it works in, which must be
    /// there as it starts (D231); none for its image's (its config's `run.workdir`, in its
    /// directory).
    pub workdir: Option<String>,
    /// Its stdin, a pipe the orchestrator writes ([`ImageGuest::take_input`]); else
    /// `/dev/null`, as every member's is today.
    pub stdin: bool,
    /// Its stdio a terminal of its own (plan step 6, D244), whose master its run's init
    /// holds: what the terminal shows is its stdout ([`ImageGuest::take_output`]), its stderr
    /// ending at once; where its stdin is asked for too, what is written to it is typed
    /// into the terminal ([`ImageGuest::take_input`]), and its end leaves the terminal open.
    /// It is 0 by 0 until [`ImageGuest::resize`] sizes it.
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
    /// The image's grants past the microVM, kept or dropped, never added to; where it holds
    /// its members apart, each member's reach of the names kept ([`Narrowing::members`],
    /// D246), which the run host refuses the run unless every member reaches exactly.
    pub egress: Narrowing,
    /// The run's credential grants (D242): each a credential named by reference (a secret
    /// in the orchestrator's store, never a value) and the hosts the injector may fill it
    /// in toward. Empty for a run with none (default deny). An orchestrator keeps or drops
    /// the image's, never adds one.
    pub credentials: Vec<CredentialGrant>,
    /// What answers the run's credential leases, for this run alone (D242): needed by a run
    /// with credential grants, which is refused without one; a run with none asks nothing.
    pub lessor: Option<RunLessor>,
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
    /// A plug or an unplug its run's host refused (D235), by what refused it.
    Plug(PlugRefusal, String),
    /// The run's init held it within no room it was told (D241), in its words.
    Room(String),
    /// The run's VM held its vCPUs to no rate it was told (D247), in its words.
    Rate(String),
    /// A member's terminal its run's init did not size (D244), in its words.
    Terminal(String),
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
            Self::Plug(cause, why) => write!(f, "the plug: {cause:?}: {why}"),
            Self::Room(why) => write!(f, "the run's room: {why}"),
            Self::Rate(why) => write!(f, "the run's rate: {why}"),
            Self::Terminal(why) => write!(f, "a member's terminal: {why}"),
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
    /// waiting at most `patience` (`Duration::MAX`: until it answers or the run ends). The
    /// run's carries what its microVM costs its host beside it ([`Stats::microvm`], D241):
    /// its VM process's CPU time and memory, and each vCPU's guest time, read before them.
    fn stats(&self, scope: StatsScope, patience: Duration) -> Result<Stats, ImageRefusal>;
    /// The room the orchestrator's budget leaves the run (D188, D241), told its init as a
    /// daemon's budget tells a run of its own: the bytes the run may use past what it uses
    /// now, every member's and its engine's ring slots among them (D230), negative where the
    /// budget is short of what it holds, which its init then reclaims of its file pages; none
    /// where no budget holds it, which lifts the hold. Its init holds the run within it, never
    /// closer to what it uses than the least room a run goes on in, so that its own kernel
    /// drops its page cache before its host must take memory its guest holds. Answers the
    /// bytes it holds the run within past what it uses, none where no budget holds it,
    /// waiting at most `patience` (`Duration::MAX`: until it answers or the run ends). The
    /// budget is the orchestrator's: it tells the room again as the budget changes or as
    /// the run's use moves, as D188's daemon does its own runs'.
    fn room(&self, room: Option<i64>, patience: Duration) -> Result<Option<u64>, ImageRefusal>;
    /// The CPU rate its orchestrator holds the run's vCPUs to together (D191, D247), in
    /// milli-CPU of the host's time, as a daemon holds a run of its own to its CPUs: once a
    /// period's share is spent, every vCPU is held out of its guest until the period ends,
    /// whatever its guest does, and what a period overran comes off the next ones. None lifts
    /// the hold, the microVM's vCPUs its ceiling. Answers what its vCPUs are held to, none
    /// where nothing holds them (no rate, or every vCPU's whole CPU or more), waiting at most
    /// `patience` (`Duration::MAX`: until it answers or the run ends). The rate is the
    /// orchestrator's: it tells it again as it moves.
    fn rate(
        &self,
        rate: Option<std::num::NonZeroU64>,
        patience: Duration,
    ) -> Result<Option<std::num::NonZeroU64>, ImageRefusal>;
    /// Member `member`'s own terminal ([`MemberSpec::terminal`]) sized to `rows` by `cols`
    /// (plan step 6, D244), as a terminal's window is: its run's init sets the size, and the
    /// kernel tells the terminal's foreground process group SIGWINCH where it changed; a 0 in
    /// either leaves it as it is. Waits at most `patience` (`Duration::MAX`: until it answers
    /// or the run ends). Refused for a member its spec gave no terminal
    /// ([`ImageRefusal::Spec`]), and as [`ImageRefusal::Terminal`] where its init did not
    /// size it.
    fn resize(
        &self,
        member: &MemberName,
        rows: u16,
        cols: u16,
        patience: Duration,
    ) -> Result<(), ImageRefusal>;
    /// The readiness of the run's own command under the nesting grant (D234), as its init
    /// last said it, once it differs from `seen` (none: once there is any), waiting at most
    /// `timeout` (`Duration::MAX`: until it comes; `Duration::ZERO`: as it stands). Its
    /// deadline passed ([`Ready::Passed`]) moves on to ready late at its `READY=1`; ready is
    /// where it stays. Refused for a run whose spec does not ask for the grant, and once the
    /// run has ended with nothing newer ([`ImageRefusal::Ended`]).
    fn wait_ready(&self, seen: Option<Ready>, timeout: Duration) -> Result<Option<Ready>, ImageRefusal>;
    /// Host directory `host` plugged into running member `member` as the run goes (D235),
    /// at `at`: absolute, strictly beneath one of the run's plug roots, which its host names
    /// as it starts (its OS's own, and the top-level directory of each of its shares), so
    /// that a project is plugged at its host path. The host's files there are the member's,
    /// read-only where asked; the plug grants nothing beyond its own path. Waits at most
    /// `patience`; refused as [`ImageRefusal::Plug`], by what refused it.
    fn plug(
        &self,
        member: &MemberName,
        host: &std::path::Path,
        at: &str,
        read_only: bool,
        patience: Duration,
    ) -> Result<(), ImageRefusal>;
    /// What is plugged in at `at` let go of: unmounted in its member, refused while anything
    /// there uses it ([`PlugRefusal::Busy`]), then taken off the microVM.
    fn unplug(&self, at: &str, patience: Duration) -> Result<(), ImageRefusal>;
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
        if let Some(w) = &m.workdir
            && !shards_abi::run::members::workdir_ok(w.as_bytes())
        {
            return Err(ImageRefusal::Spec {
                what: "a member's working directory",
                why: format!("{w:?}: {}", shards_abi::run::members::Error::Workdir),
            });
        }
        members.push(wire::MemberStage {
            kind: m.member.kind,
            name: m.member.name.clone(),
            env,
            files,
            workdir: m.workdir.clone(),
            input: m.stdin,
            terminal: m.terminal,
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
    // The run's credential grants (D242): a reference names a secret, never a value, and
    // each host it is filled in toward is a DNS name as D236 names a destination's host by
    // name, or host.docker.internal, reusing shards_dns' RFC rules; never an address, never a
    // port, and never localhost. The hosts are stored lowered, so the
    // injector's case-insensitive decision rests on a lowered store.
    let mut credentials = Vec::with_capacity(spec.credentials.len());
    for grant in &spec.credentials {
        if grant.reference.is_empty() {
            return Err(ImageRefusal::Spec {
                what: "a credential grant",
                why: "a grant names a credential by reference, which is empty".into(),
            });
        }
        if grant.toward.is_empty() {
            return Err(ImageRefusal::Spec {
                what: "a credential grant",
                why: format!(
                    "{}: a grant toward no host could never be filled",
                    grant.reference
                ),
            });
        }
        let mut toward = Vec::with_capacity(grant.toward.len());
        for host in &grant.toward {
            let lowered = host.to_ascii_lowercase();
            // A DNS name alone: shards_dns' rules refuse an address's all-digit last label,
            // and an address could never be filled toward, as TLS names a server by its DNS
            // name only (RFC 6066 §3) and the injector fills where the SNI names the host.
            if !shards_dns::host_name(&lowered) || lowered == "localhost" || lowered.ends_with(".localhost") {
                return Err(ImageRefusal::Spec {
                    what: "a credential grant",
                    why: format!(
                        "{}: {host} is not a host a credential is filled in toward: a DNS name, or {}, \
                         without a port and never localhost (TLS names a server by its DNS name alone, \
                         RFC 6066 §3)",
                        grant.reference,
                        shards_dns::HOST
                    ),
                });
            }
            toward.push(lowered);
        }
        credentials.push(wire::CredentialGrant {
            reference: grant.reference.clone(),
            toward,
        });
    }
    // Its grants' values come by lease alone (D242): a run with grants and nothing to lease
    // them could never fill one.
    if !credentials.is_empty() && spec.lessor.is_none() {
        return Err(ImageRefusal::Spec {
            what: "the run's lessor",
            why: "a run with credential grants needs a lessor to lease their values".into(),
        });
    }
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
        credentials,
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
                workdir: Some("/Users/ada/proj".into()),
                stdin: false,
                terminal: false,
            }],
            publish: Vec::new(),
            egress: Narrowing::default(),
            credentials: Vec::new(),
            lessor: None,
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

    /// A lessor that leases nothing.
    struct Leasing;

    impl Lessor for Leasing {
        fn lease(&self, _: &str) -> Result<Leased, LeaseRefusal> {
            Err(LeaseRefusal::Denied("this test's".into()))
        }

        fn release(&self, _: &str) {}
    }

    fn lessor() -> Option<RunLessor> {
        Some(RunLessor(Arc::new(Leasing)))
    }

    /// A spec becomes the run host's request, a variable `KEY=VALUE`.
    #[test]
    fn a_spec_is_the_request_the_host_reads() {
        let r = request(&spec()).unwrap();
        assert_eq!(r.members[0].env, vec![Secret::from("OPENAI_API_KEY=sk-1")]);
        assert_eq!(r.members[0].files[0].mode, 0o600);
        assert_eq!(r.members[0].workdir.as_deref(), Some("/Users/ada/proj"));
        assert!(r.shares[0].read_only);
        assert_eq!((r.nesting, r.ready_by), (false, None));
        assert!(r.credentials.is_empty());
        assert_eq!(wire::Request::decode(&r.encode().unwrap()).unwrap(), r);
    }

    /// A credential grant rides the request by reference, its hosts lowered; a bad host or
    /// an empty reference is refused, and no grant means none (default deny).
    #[test]
    fn a_credential_grant_rides_by_reference() {
        let mut spec = spec();
        spec.credentials = vec![CredentialGrant {
            reference: "anthropic/claude-max".into(),
            toward: vec!["API.anthropic.com".into(), "platform.claude.com".into()],
        }];
        spec.lessor = lessor();
        let r = request(&spec).unwrap();
        assert_eq!(r.credentials.len(), 1);
        assert_eq!(r.credentials[0].reference, "anthropic/claude-max");
        // Stored lowered, so the injector's case-insensitive match rests on a lowered store.
        assert_eq!(
            r.credentials[0].toward,
            vec!["api.anthropic.com", "platform.claude.com"]
        );
        assert_eq!(wire::Request::decode(&r.encode().unwrap()).unwrap(), r);
        // An empty reference is refused.
        let mut bad = spec.clone();
        bad.credentials[0].reference = String::new();
        assert!(matches!(request(&bad), Err(ImageRefusal::Spec { .. })));
        // A grant toward no host, which could never be filled, is refused.
        let mut bad = spec.clone();
        bad.credentials[0].toward.clear();
        assert!(matches!(request(&bad), Err(ImageRefusal::Spec { .. })));
        // A host with a port, localhost, or an address (which no SNI names, RFC 6066 §3) is
        // not a host a credential fills toward.
        for host in [
            "api.anthropic.com:443",
            "localhost",
            "under.localhost",
            "10.0.0.5",
            "::1",
        ] {
            let mut bad = spec.clone();
            bad.credentials[0].toward = vec![host.into()];
            assert!(
                matches!(request(&bad), Err(ImageRefusal::Spec { .. })),
                "{host} was accepted as a credential's host"
            );
        }
        // host.docker.internal, the host itself, is one.
        let mut ok = spec.clone();
        ok.credentials[0].toward = vec!["host.docker.internal".into()];
        assert!(request(&ok).is_ok(), "host.docker.internal was refused");
    }

    /// Do: ask a run with credential grants and no lessor, with one, and a run with neither
    /// grants nor lessor, and one with a lessor and no grants. Expect: the first refused by
    /// field, before anything starts, as its grants could never be filled (D242); the others
    /// asked as given, a run with no grant asking nothing of its lessor.
    #[test]
    fn a_run_with_credential_grants_needs_a_lessor() {
        let mut granted = spec();
        granted.credentials = vec![CredentialGrant {
            reference: "anthropic/claude-max".into(),
            toward: vec!["api.anthropic.com".into()],
        }];
        assert!(matches!(
            request(&granted),
            Err(ImageRefusal::Spec {
                what: "the run's lessor",
                ..
            })
        ));
        granted.lessor = lessor();
        assert!(request(&granted).is_ok());
        assert!(request(&spec()).is_ok());
        let mut idle = spec();
        idle.lessor = lessor();
        assert!(request(&idle).unwrap().credentials.is_empty());
        // A spec's lessor is the same where it is the same one, and never prints more than
        // its name.
        assert_eq!(granted.lessor, granted.clone().lessor);
        assert_ne!(granted.lessor, lessor());
        assert_eq!(format!("{:?}", granted.lessor), "Some(RunLessor)");
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
        for relative_or_escaping in [
            "proj",
            "./proj",
            "/Users/ada/../../etc",
            "/Users//ada",
            "/Users/ada/",
        ] {
            let mut s = spec();
            s.members[0].workdir = Some(relative_or_escaping.into());
            assert_eq!(
                refused(s),
                "a member's working directory",
                "{relative_or_escaping}"
            );
        }
        // A member's terminal (D244) and its stdin are asked for in its request, as the run
        // host takes them.
        let mut s = spec();
        s.members[0].terminal = true;
        assert!(request(&s).unwrap().members[0].terminal);
        assert!(!request(&spec()).unwrap().members[0].terminal);
        let mut s = spec();
        s.members[0].stdin = true;
        assert!(request(&s).unwrap().members[0].input);
        assert!(!request(&spec()).unwrap().members[0].input);
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
