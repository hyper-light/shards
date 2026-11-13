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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

mod pool;
mod session;
pub mod wire;

pub use wire::{
    Bound, Captured, Channel, Cli, ControlAsk, ControlAt, CredentialGrant, Fill, GuestTimes, HostUse, Keep,
    LeaseRefusal, MemberInfo, MemberKind, MemberReach, Microvm, Narrowing, PlugRefusal, Proto, Publish,
    Ready, Receiving, RunEnd, SaveRefusal, Secret, SignInGrant, SignInRefusal, SignInTokens, SocketGrant,
    StartedFrom, Stats, StatsScope, VcpuTime, decide,
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
/// reference, through the run's host. The run is the one its lessor was given for, its asks
/// coming on its own channel; its leases are issued to its own key where it has one. Every
/// lease it gives is released as the run ends, by whatever end.
pub trait Lessor: Send + Sync {
    /// Credential `reference`'s value, leased to the run now, or why not, typed. Called on a
    /// thread of its own, once for each ask, `reference` one of the run's grants. `key` is
    /// the SHA-256 of the run's own key, where it binds an agent-control endpoint (D252: its
    /// `ControlAt::vm`, told before any ask): the identity its leases are issued to (D238),
    /// which its broker alone says is current, refusing another as `KeyReplaced`; none where
    /// the run has no key, its lessor's run alone asking.
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Leased, LeaseRefusal>;
    /// The lease [`lease`](Self::lease) named `lease`, released: the run has ended, or the
    /// lease was given after it did or ended before it could be passed on, or the value it
    /// gave rotated (its sign-in refreshed, [`refresh`](Self::refresh)). Called once for each
    /// lease given.
    fn release(&self, lease: &str);
    /// Sign-in `sign_in`'s tokens, as the run's injector took them from its CLI's token
    /// response (D242, step 8): kept outside by the run's broker, which gives back the
    /// placeholders the CLI holds in their places ([`Captured`]: its access token's reference,
    /// which the run's leases name from then on as a grant's, that token's placeholder, and its
    /// refresh token's where `tokens` has one). Called on a thread of its own, once for each
    /// capture, `sign_in` one of the run's sign-in grants, `key` as [`lease`](Self::lease) is
    /// given it. An orchestrator that serves no sign-in refuses it `Unanswered`, as this does.
    fn capture(
        &self,
        _sign_in: &str,
        _tokens: SignInTokens,
        _key: Option<[u8; 32]>,
    ) -> Result<Captured, SignInRefusal> {
        Err(SignInRefusal::Unanswered)
    }
    /// Sign-in `sign_in` refreshed by its broker, which alone calls its provider (Q1), once for
    /// every run of it at a time (RFC 6749 §6), keeping the rotated tokens under the references
    /// its capture gave: the token response its CLI is to have, as its provider gave it or with
    /// the sign-in's placeholders in its tokens' places, as the run's injector gives them those
    /// whatever it holds (Q2: they stay). Called on a thread of its own, once for each ask, every
    /// flow of the run that asked one joined to it; once it answers, each lease of the access
    /// token given the run is released ([`release`](Self::release)), its value rotated. Refused
    /// `Unanswered` by an orchestrator that serves no sign-in, as by this.
    fn refresh(&self, _sign_in: &str, _key: Option<[u8; 32]>) -> Result<Secret, SignInRefusal> {
        Err(SignInRefusal::Unanswered)
    }
    /// The run's VM revoked sign-in `sign_in` (its CLI's logout, or a sign-in anew): answered
    /// in the run's network process, its stored tokens kept. Told, on a thread of its own, so
    /// that its broker may say so; never a revocation of the sign-in outside.
    fn revoked(&self, _sign_in: &str, _key: Option<[u8; 32]>) {}
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
    /// Its run-time arguments (D256), in their order: its command line is its image's
    /// `run.command` (its startup descriptor's entrypoint and options, `agent.json` or
    /// `harness.json`), then these, so that a harness takes its session's own flags (Claude
    /// Code's `--resume`, `--model`) as it does on the host. None: its image's command alone.
    /// No argument holds a NUL. A command line is no secret, read by whatever sees the member's
    /// processes (proc(5), `/proc/PID/cmdline`): a credential goes in [`env`](Self::env) or
    /// [`files`](Self::files). Bounded with the rest of the spec by the channel's own bound,
    /// `shards_abi::run::MAX_PAYLOAD`, Linux's own on a command line and environment, and
    /// refused past it, never cut.
    pub args: Vec<String>,
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
    /// in the orchestrator's store, never a value), the hosts the injector may fill it in
    /// toward, and the placeholder the orchestrator gives the member that uses it, which the
    /// injector fills the value in where it stands. Empty for a run with none (default
    /// deny). An orchestrator keeps or drops the image's, never adds one.
    pub credentials: Vec<CredentialGrant>,
    /// What answers the run's credential leases, for this run alone (D242): needed by a run
    /// with credential grants, which is refused without one; a run with none asks nothing.
    pub lessor: Option<RunLessor>,
    /// The run's sign-in grants (D242, step 8): each a sign-in its CLI may sign in as (its name
    /// in the orchestrator's store), the hosts its access token goes to, and the placeholders
    /// an earlier capture of it gave, staged by the orchestrator into its CLI's file. Its
    /// tokens are taken outside the microVM by the run's injector and kept by its
    /// [`lessor`](Self::lessor), needed by a run with any. One sign-in of a CLI a run. Empty
    /// for a run with none (default deny).
    pub sign_ins: Vec<SignInGrant>,
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
    /// The run's agent-control endpoint (D238, D252), bound by its host for this run alone,
    /// its key the run's own, made after its restore and held by the endpoint alone; it takes
    /// [`ControlAsk::controller`]'s key alone, and serves lanes to the run's own command's
    /// sockets where [`ControlAsk::sockets`] grants them (D251). None: no endpoint.
    pub control: Option<ControlAsk>,
    /// The host GPUs the run's VM renders on, each by its exact identity (D232, as
    /// [`crate::GuestSpec::gpu_devices`] names them), bound by its VM process; under the
    /// nesting grant its command reads them in `/run/shards/gpus` (D234's `gpus=`). None: no
    /// GPU.
    pub gpu_devices: Vec<crate::PhysicalGpu>,
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
    /// A save its run's host refused (D259), by what refused it.
    Save(SaveRefusal, String),
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
            Self::Save(cause, why) => write!(f, "the save: {cause:?}: {why}"),
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
    /// [`boot`](Self::boot), ended by `stop` (D231's stop of a boot under way): a
    /// [`BootStop::stop`] from any thread while it boots kills the run's host and every
    /// process it started, and the boot returns [`ImageRefusal::Ended`]`(`[`RunEnd::Stopped`]`)`
    /// once they are gone; a stop that came before is the same, booting nothing. A stop that
    /// comes after the boot has returned is no longer the boot's: the guest's own
    /// [`stop`](ImageGuest::stop) ends its run.
    ///
    /// By default, for an implementor with no run host to end mid-boot, the stop is honoured
    /// as the boot returns: its guest stopped, and `Ended(Stopped)` returned.
    fn boot_stoppable(
        &self,
        spec: &ImageSpec,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        if stop.is_stopped() {
            return Err(ImageRefusal::Ended(RunEnd::Stopped));
        }
        let guest = self.boot(spec, patience)?;
        if stop.is_stopped() {
            let _ = guest.stop();
            return Err(ImageRefusal::Ended(RunEnd::Stopped));
        }
        Ok(guest)
    }
}

/// A stop for a boot under way ([`Images::boot_stoppable`], D231): its caller's, given to one
/// boot at a time by reference and stopped from any other thread. Its stop is an event, never
/// a flag a boot looks at: it kills the run host the boot holds here at once, every process
/// that host started with it, and the boot, which waits on that host's end, returns as the
/// host is followed to its end. Once stopped it stays stopped.
#[derive(Debug, Default)]
pub struct BootStop(Mutex<Stopping>);

/// A [`BootStop`]'s state (R04): `Armed` to `Booting` as a boot holds its run host here and
/// back as the boot lets go of it; either to `Stopped`, killing a host held, which a boot
/// holding a host after finds and kills at once.
#[derive(Debug, Default)]
enum Stopping {
    /// Not stopped, and no boot holds a run host here.
    #[default]
    Armed,
    /// A boot holds its run host here while its run starts: the host the boot's own
    /// session and its exit watcher already share.
    Booting(Arc<shards_ipc::Child>),
    /// Stopped: terminal.
    Stopped,
}

impl BootStop {
    pub fn new() -> BootStop {
        BootStop::default()
    }

    /// Stops the boot under way, killing its run host and all the host started, or the next
    /// boot given it. Never refused; once is enough. Returns at once: the boot returns
    /// `Ended(Stopped)` once they are gone.
    pub fn stop(&self) {
        let mut state = self.state();
        if let Stopping::Booting(host) = std::mem::replace(&mut *state, Stopping::Stopped) {
            session::kill(&host);
        }
    }

    /// Whether it has been stopped.
    pub fn is_stopped(&self) -> bool {
        matches!(*self.state(), Stopping::Stopped)
    }

    /// A boot's run `host`, held here until [`release`](Self::release): killed at once where
    /// the stop has come. Refused where another boot holds one here.
    fn hold(&self, host: &Arc<shards_ipc::Child>) -> Result<(), ImageRefusal> {
        let mut state = self.state();
        match *state {
            Stopping::Armed => *state = Stopping::Booting(host.clone()),
            Stopping::Booting(_) => {
                return Err(ImageRefusal::Spec {
                    what: "the boot's stop",
                    why: "another boot under way holds it".into(),
                });
            }
            Stopping::Stopped => session::kill(host),
        }
        Ok(())
    }

    /// Lets go of the host a boot held here: whether the stop came first.
    fn release(&self) -> bool {
        let mut state = self.state();
        if matches!(*state, Stopping::Stopped) {
            return true;
        }
        *state = Stopping::Armed;
        false
    }

    fn state(&self) -> MutexGuard<'_, Stopping> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A booted image. Dropping it ends it.
pub trait ImageGuest: Send + Sync {
    /// Every member the image declares, by its declared index.
    fn members(&self) -> &[MemberInfo];
    /// Each port published, as bound, and the members it reaches ([`Bound::to`]).
    fn published(&self) -> &[Bound];
    /// Where the run's agent-control endpoint is bound, and its microVM's key, which its
    /// controller pins (D252), where [`ImageSpec::control`] asked for one: as the run started,
    /// unchanged until it ends.
    fn control(&self) -> Option<&ControlAt>;
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
    /// The period the run's VM's governor holds a [`rate`](Self::rate) over (D191, D247):
    /// each period the run's vCPUs together use at most the rate's share of it, held out of
    /// their guest once it is spent until the period ends. As its run host said it as the run
    /// started, the period its own build's hold uses, so that an orchestrator derives its
    /// window from the run's own and copies no number: a period shards derives later (D191)
    /// reaches it as it is.
    fn rate_period(&self) -> Duration;
    /// How the run's microVM started (D253): restored from its image's template, booted saving
    /// one, or booted where its template did not restore, which was then removed; booted alone
    /// where its host had no template to restore or save. As its run host said it as the run
    /// started.
    fn started_from(&self) -> StartedFrom;
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
    /// The run's microVM saved whole and left running (D176, D259): its memory, its vCPUs'
    /// and devices' state, its run's swap and its writable layer, as the daemon's `save vm`
    /// saves one, in a snapshot directory (`shards_vmm::snapshot`'s: `current` naming the
    /// generation, whose `memory`, `state`, `swap`, `layer-head`, `layer` and `window-heads`
    /// they are) at `to`, which the save makes: absolute, beneath the run's home (its
    /// `SHARDS_HOME`), beneath no directory its guest reaches (its shares, what is plugged
    /// into it, the home's volumes), and with nothing there yet. It holds every value its
    /// guest holds: it is its owner's alone, asked on the run's own channel and written
    /// nowhere else, and its owner's to remove. Waits at most `patience`; refused as
    /// [`ImageRefusal::Save`], by what refused it: a path not UTF-8 as [`ImageRefusal::Spec`].
    fn save(&self, to: &std::path::Path, patience: Duration) -> Result<(), ImageRefusal>;
    /// Ends it at once: its VM's process is killed, and [`wait_for`](Self::wait_for) then
    /// returns [`RunEnd::Stopped`], or the end the run had come to before: its command's, or
    /// its host's failure, in its words, as `wait_for` saw it. Never refused, as a VM left
    /// running is a leak. Returns once the run's end is told, its VM gone; its host then
    /// reaps what it started and exits on its own.
    fn stop(&self) -> Result<(), ImageRefusal>;
    /// Its end, once it has come, waiting at most `timeout` (`Duration::MAX`: until it
    /// comes).
    fn wait_for(&self, timeout: Duration) -> Result<Option<RunEnd>, ImageRefusal>;
    /// The run host's diagnostics, its VM's among them, written whole to `to` from now on, as
    /// the host writes them, in place of any file named before, after the last of them kept
    /// until now: what a refusal or a failed end quotes ([`ImageRefusal::Boot`],
    /// [`RunEnd::Failed`]) is only their last. A file that stalls holds them up; a part it
    /// refuses is lost to it alone.
    fn keep_diagnostics(&self, to: std::fs::File) -> Result<(), ImageRefusal>;
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
        self.boot_stoppable(spec, patience, &BootStop::new())
    }

    fn boot_stoppable(
        &self,
        spec: &ImageSpec,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        if stop.is_stopped() {
            return Err(ImageRefusal::Ended(RunEnd::Stopped));
        }
        crate::backend().map_err(no_backend)?;
        let request = request(spec)?;
        Ok(Box::new(session::Session::start(spec, &request, patience, stop)?))
    }
}

/// The images the `shards` binary boots, each run on a warm run host of its pool where one
/// is ready (D253's pools, D26 through shards-api): a run host started ahead of its run, its
/// microVM restored from its image's template and ready, waiting for the run's request, as
/// the daemon's warm VMs wait for theirs. A pool is the runs of one microVM, alike in all but
/// their members' stages, deadline and control ask ([`wire::Request::exemplar`]); each keeps
/// the largest burst of its runs within one refill window while in use, at least one, and
/// none once idle past its keep-alive (`shards_api::demand`, the daemon's own rule). Nothing
/// is warm until a pool's first run, which starts a host of its own, as [`ImageShards`]
/// would. Dropping it ends every warm host.
#[derive(Debug)]
pub struct ImagePools(pool::Pooling<session::ShardsHosts>);

impl ImagePools {
    /// D26's keep-alive, which the daemon's pools keep: as AWS keeps an idle function, 10
    /// minutes (Shahrad et al., "Serverless in the Wild", USENIX ATC 2020, §1).
    pub const KEEP: Duration = crate::demand::KEEP;

    /// Pools that all together keep at most `most` warm run hosts, the orchestrator's bound,
    /// each pool keeping its hosts `keep` once idle ([`KEEP`](Self::KEEP) is D26's).
    pub fn new(most: usize, keep: Duration) -> Result<ImagePools, ImageRefusal> {
        Ok(ImagePools(pool::Pooling::new(session::ShardsHosts, most, keep)?))
    }
}

impl Images for ImagePools {
    fn backend(&self) -> Result<crate::Backend, ImageRefusal> {
        crate::backend().map_err(no_backend)
    }

    fn boot(&self, spec: &ImageSpec, patience: Duration) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        self.boot_stoppable(spec, patience, &BootStop::new())
    }

    fn boot_stoppable(
        &self,
        spec: &ImageSpec,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        if stop.is_stopped() {
            return Err(ImageRefusal::Ended(RunEnd::Stopped));
        }
        crate::backend().map_err(no_backend)?;
        let request = request(spec)?;
        self.0.boot(spec, &request, patience, stop)
    }
}

fn no_backend(r: crate::Refusal) -> ImageRefusal {
    match r {
        crate::Refusal::NoBackend { os, arch } => ImageRefusal::NoBackend { os, arch },
        other => ImageRefusal::Boot(other.to_string()),
    }
}

/// The run's sign-in grants (D242, step 8), as its request carries them: each names its sign-in
/// and the hosts its access token goes to, each host a DNS name as a credential grant's are
/// (lowered, never an address, a port or localhost); one sign-in of a CLI a run, as a capture
/// names no sign-in; each placeholder an earlier capture gave a token, and one credential's alone,
/// the run's credential grants' included; and a lessor to keep their tokens.
fn sign_ins(
    spec: &ImageSpec,
    credentials: &[wire::CredentialGrant],
) -> Result<Vec<SignInGrant>, ImageRefusal> {
    let refuse = |why: String| ImageRefusal::Spec {
        what: "a sign-in grant",
        why,
    };
    let mut sign_ins: Vec<SignInGrant> = Vec::with_capacity(spec.sign_ins.len());
    for s in &spec.sign_ins {
        if s.sign_in.is_empty() {
            return Err(refuse("a grant names its sign-in, which is empty".into()));
        }
        if s.toward.is_empty() {
            return Err(refuse(format!(
                "{}: a sign-in whose access token goes to no host could never be used",
                s.sign_in
            )));
        }
        if sign_ins.iter().any(|o| o.cli == s.cli) {
            return Err(refuse(format!(
                "{}: a second sign-in of its CLI, where a capture names no sign-in",
                s.sign_in
            )));
        }
        let mut toward = Vec::with_capacity(s.toward.len());
        for host in &s.toward {
            let lowered = host.to_ascii_lowercase();
            if !shards_dns::host_name(&lowered) || lowered == "localhost" || lowered.ends_with(".localhost") {
                return Err(refuse(format!(
                    "{}: {host} is not a host a token is filled in toward: a DNS name, or {}, without a \
                     port and never localhost (TLS names a server by its DNS name alone, RFC 6066 §3)",
                    s.sign_in,
                    shards_dns::HOST
                )));
            }
            toward.push(lowered);
        }
        sign_ins.push(SignInGrant {
            sign_in: s.sign_in.clone(),
            cli: s.cli,
            toward,
            captured: s.captured.clone(),
        });
    }
    shards_dns::credential::check_sign_ins(credentials, &sign_ins)
        .map_err(|refused| refuse(refused.to_string()))?;
    if !sign_ins.is_empty() && spec.lessor.is_none() {
        return Err(ImageRefusal::Spec {
            what: "the run's lessor",
            why: "a run with sign-in grants needs a lessor to keep their tokens".into(),
        });
    }
    Ok(sign_ins)
}

/// `spec` as the run host reads it, refused here where it could never be run: a member's
/// terminal (owed, G24), a share's path that is no UTF-8 or is relative, a variable's key
/// that holds `=` or a NUL, an argument that holds a NUL, or a key or file the guest would
/// refuse.
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
        // Its run-time arguments (D256), each told by its place, never its bytes.
        for (i, a) in m.args.iter().enumerate() {
            shards_abi::run::members::arg_ok(a.as_bytes()).map_err(|e| ImageRefusal::Spec {
                what: "a member's argument",
                why: format!("{}: its argument {i}: {e}", m.member.name),
            })?;
        }
        members.push(wire::MemberStage {
            kind: m.member.kind,
            name: m.member.name.clone(),
            env,
            files,
            workdir: m.workdir.clone(),
            input: m.stdin,
            terminal: m.terminal,
            args: m.args.clone(),
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
    // injector's case-insensitive decision rests on a lowered store. Each placeholder is one
    // credential's alone.
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
            placeholder: grant.placeholder.clone(),
        });
    }
    // Each placeholder stands where its member sends the credential: a bearer token, and no
    // other credential's, so that the injector knows which value it is.
    shards_dns::credential::check(&credentials).map_err(|refused| ImageRefusal::Spec {
        what: "a credential grant",
        why: refused.to_string(),
    })?;
    // Its grants' values come by lease alone (D242): a run with grants and nothing to lease
    // them could never fill one.
    if !credentials.is_empty() && spec.lessor.is_none() {
        return Err(ImageRefusal::Spec {
            what: "the run's lessor",
            why: "a run with credential grants needs a lessor to lease their values".into(),
        });
    }
    let sign_ins = sign_ins(spec, &credentials)?;
    // The run's agent-control endpoint (D252): its socket grant's directory absolute and plain,
    // as a member's working directory is, refused here, by the run host and by init; its
    // admission one lane and one byte at least (R03: the caller's numbers), and the grant a run's
    // own command's, so none in a run of its image's members alone.
    if let Some(grant) = spec.control.as_ref().and_then(|c| c.sockets.as_ref()) {
        let refuse = |why: String| ImageRefusal::Spec {
            what: "the socket grant",
            why,
        };
        if !shards_abi::run::socket::dir_ok(grant.dir.as_bytes()) {
            return Err(refuse(format!(
                "{:?}: {}",
                grant.dir,
                shards_abi::run::socket::Error::Dir
            )));
        }
        if grant.lanes == 0 || grant.window == 0 {
            return Err(refuse(format!(
                "{} lanes of {} bytes: a grant admits a lane and a byte at least",
                grant.lanes, grant.window
            )));
        }
        if spec.command.is_none() {
            return Err(refuse(
                "a run without a command of its own has no sockets to grant".into(),
            ));
        }
    }
    // The run's GPUs (D232): each a host GPU's exact identity, none twice, within the bound the
    // renderer's selection holds, as the VM process checks them again.
    shards_abi::gpu::check_selection(&spec.gpu_devices).map_err(|e| ImageRefusal::Spec {
        what: "the GPU selection",
        why: e.to_string(),
    })?;
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
        control: spec.control.clone(),
        gpu_devices: spec.gpu_devices.clone(),
        sign_ins,
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

    /// A placeholder as kiln's broker mints one: a visible marker, then 256 random bits in hex.
    const PLACEHOLDER: &str =
        "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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
                args: Vec::new(),
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
            control: None,
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
        }
    }

    /// A lessor that leases nothing.
    struct Leasing;

    impl Lessor for Leasing {
        fn lease(&self, _: &str, _: Option<[u8; 32]>) -> Result<Leased, LeaseRefusal> {
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

    /// A member's run-time arguments (D256) ride its request as given, in their order, an
    /// empty one among them; a member given none has none, its image's command alone.
    #[test]
    fn a_members_arguments_ride_its_request_in_order() {
        assert!(request(&spec()).unwrap().members[0].args.is_empty());
        let mut s = spec();
        s.members[0].args = vec![
            "--resume".into(),
            "s-1".into(),
            String::new(),
            "--model".into(),
            "claude-opus-4".into(),
        ];
        let r = request(&s).unwrap();
        assert_eq!(r.members[0].args, s.members[0].args);
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
            placeholder: PLACEHOLDER.into(),
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
        // Its placeholder rides with it; one that is no bearer token is refused.
        assert_eq!(r.credentials[0].placeholder, PLACEHOLDER);
        for placeholder in ["", "two words", "a:b"] {
            let mut bad = spec.clone();
            bad.credentials[0].placeholder = placeholder.into();
            assert!(
                matches!(request(&bad), Err(ImageRefusal::Spec { .. })),
                "{placeholder:?} was taken as a placeholder"
            );
        }
        // One placeholder for two credentials is refused, as the injector could not tell
        // which it stands for; one credential granted twice with it is two grants of one.
        let mut twice = spec.clone();
        twice.credentials.push(CredentialGrant {
            reference: "anthropic/console".into(),
            ..spec.credentials[0].clone()
        });
        assert!(matches!(request(&twice), Err(ImageRefusal::Spec { .. })));
        twice.credentials[1].reference = "anthropic/claude-max".into();
        assert!(request(&twice).is_ok());
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
            placeholder: PLACEHOLDER.into(),
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
        // An argument holding a NUL, which no command line exec'd holds (D256), told by its
        // place alone.
        let mut s = spec();
        s.members[0].args = vec!["--resume".into(), "s-1\0--model".into()];
        match request(&s) {
            Err(ImageRefusal::Spec { what, why }) => {
                assert_eq!(what, "a member's argument");
                assert!(why.contains("argument 1") && !why.contains("s-1"), "{why}");
            }
            other => panic!("{other:?}"),
        }
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

    /// A spec past the channel's bound is refused before the run host starts: a file's bytes,
    /// or a member's arguments (D256), never cut to fit.
    #[test]
    fn a_spec_past_the_bound_is_refused() {
        let mut s = spec();
        s.members[0].files[0].bytes = Secret(vec![0; shards_abi::run::MAX_PAYLOAD as usize]);
        assert!(matches!(
            request(&s),
            Err(ImageRefusal::Spec { what: "the spec", .. })
        ));
        let half = shards_abi::run::MAX_PAYLOAD as usize / 2;
        let mut s = spec();
        s.members[0].args = vec!["x".repeat(half), "x".repeat(half)];
        assert!(matches!(
            request(&s),
            Err(ImageRefusal::Spec { what: "the spec", .. })
        ));
    }

    /// Step 11 (b), D252 (Hex's acceptance): a socket grant that names no plain absolute
    /// directory, admits no lane or no byte, or is asked of a run without a command of its own,
    /// is refused by shards-api before any run host starts, as a member's working directory is;
    /// a grant within its bounds is carried whole.
    #[test]
    fn a_socket_grant_out_of_its_bounds_is_refused_before_a_run_host_starts() {
        let grant = |dir: &str, lanes, window| ControlAsk {
            controller: [1u8; 32],
            sockets: Some(SocketGrant {
                dir: dir.into(),
                lanes,
                window,
            }),
        };
        for (dir, lanes, window) in [
            ("run/hex/channel", 8, 65_536),
            ("", 8, 65_536),
            ("/run/hex/../etc", 8, 65_536),
            ("/run//hex", 8, 65_536),
            ("/run/hex/./channel", 8, 65_536),
            ("/run/hex/channel", 0, 65_536),
            ("/run/hex/channel", 8, 0),
        ] {
            let mut s = spec();
            s.nesting = true;
            s.command = Some(vec!["/init".into()]);
            s.control = Some(grant(dir, lanes, window));
            assert!(
                matches!(request(&s), Err(ImageRefusal::Spec { .. })),
                "{dir:?} lanes {lanes} window {window}"
            );
        }
        let mut s = spec();
        s.command = Some(vec!["/init".into()]);
        s.control = Some(grant("/run/hex/channel", 8, 65_536));
        assert_eq!(request(&s).unwrap().control, s.control);
        s.command = None;
        assert!(matches!(request(&s), Err(ImageRefusal::Spec { .. })));
        s.control = Some(ControlAsk {
            controller: [1u8; 32],
            sockets: None,
        });
        assert_eq!(request(&s).unwrap().control, s.control);
    }

    /// D232 through D231: a run's GPUs are D232's selection, checked before any run host starts
    /// (one GPU twice refused), and carried whole.
    #[test]
    fn a_runs_gpus_are_checked_before_a_run_host_starts() {
        let gpu = crate::PhysicalGpu::MetalRegistry(0x1_0000_02a1);
        let mut s = spec();
        s.gpu_devices = vec![gpu];
        assert_eq!(request(&s).unwrap().gpu_devices, vec![gpu]);
        s.gpu_devices = vec![gpu, gpu];
        assert!(matches!(request(&s), Err(ImageRefusal::Spec { .. })));
    }
}
