//! The channel between an image boot ([`super::ImageShards`]) and its run's host, `shards
//! api-run` (D231): frames on the run host's stdin and stdout with the run protocol's header
//! (a kind, three zero bytes, a big-endian u32 length; `shards_abi::run::header`), each
//! payload at most `shards_abi::run::MAX_PAYLOAD`. Integers are big-endian; a list is a u32
//! count then its items; bytes and text are a u32 length then themselves, text UTF-8. A
//! payload is read whole or refused: a field past its end, bytes after its last field, an
//! unknown tag or a bad UTF-8 string is a [`WireError`], never a default.
//!
//! Credentials ride only in [`Request`] and in a lease's answer ([`LeaseAnswer`]), each in a
//! [`Secret`], whose `Debug` prints its length alone and whose bytes are wiped as it drops.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU64;

use shards_abi::run;
pub use shards_abi::run::nesting::Ready;

/// The channel's version, the first byte of [`Request`]: changed only by appending. 2
/// appends the nesting grant's consent and deadline (D234); 3, the destinations kept
/// ([`Narrowing::destinations`], D236), and to each published port in [`Started`], the
/// members it reaches; 4, a host directory plugged into a running member and let go of
/// (`PLUG`, `UNPLUG`, answered `PLUGGED`; D235), so that a side that has no plugs refuses
/// the run as it is asked, before any plug is; 5, each member's working directory
/// ([`MemberStage::workdir`]); 6, whether each member's stdin is the orchestrator's
/// ([`MemberStage::input`], plan step 5); 7, the room its orchestrator's budget leaves the
/// run (`ROOM`, answered `HELD`; D188, D241), so that a side that tells none refuses the run
/// as it is asked, before any room is: a host of version 6 ends a run at a frame it has no
/// kind for; and the run's stats carry what its microVM costs its host and what its vCPUs ran
/// of its guest ([`Stats::microvm`], D241); 8, the run's credential grants
/// ([`Request::credentials`], D242), each a credential named by reference and the hosts it
/// may be filled in toward; 11, each member's own terminal ([`MemberStage::terminal`]) and
/// its size (`RESIZE`, answered `RESIZED`; D244), so that a side that has no terminals refuses
/// the run as it is asked, before any resize is; 12, each member's reach of the names kept
/// ([`Narrowing::members`], D246), which the run host holds every member the image declares
/// to before its microVM starts; 13, the CPU rate its orchestrator holds the run's vCPUs to
/// (`RATE`, answered `RATED`; D191, D247), so that a side that holds none refuses the run as
/// it is asked, before any rate is: a host of an earlier version ends a run at a frame it has
/// no kind for; 14, why a vCPU's guest time went unread, typed ([`VcpuTime::unread`],
/// `shards_abi::guest_time::Unread`, guest-cpu-time-v1's R04): its cause and code in their text
/// form, which an earlier version's free words never parse as; 15, the run's agent-control
/// endpoint ([`Request::control`], [`Started::control`]; D238, D251, D252), so that a side that
/// binds none refuses the run as it is asked; 16, the host GPUs the run names
/// ([`Request::gpu_devices`], D232, D234's `gpus=`), so that a side that binds none starts no
/// run without them; 17, the period the run's governor holds its rate over
/// ([`Started::rate_period`], D191, D247), so that its orchestrator derives its window from
/// the run's own period, never from a number of its own; 18, each member's run-time arguments
/// ([`MemberStage::args`], D256), so that a side that passes none starts no member without
/// the flags its orchestrator asked; 19, how the run's microVM started ([`Started::from`],
/// D253): from its image's template, booted saving one, or booted where its template did not
/// restore, so that its orchestrator hears a template replaced; 20, a credential's lease, asked by the run host
/// just in time (`LEASE`, answered `LEASED`), and the run's release of every lease
/// (`RELEASED`; D242), so that a side that leases nothing refuses the run as it is asked,
/// before any lease is: a client of an earlier version ends a run at a frame it has no kind
/// for; 21, warm run hosts ([`to_host::WARM`], [`to_client::WARMED`], [`Cause::Unfit`],
/// [`StartedFrom::Warm`]; D253's pools), so that a side that keeps none is never asked to;
/// 22, each credential grant's placeholder ([`CredentialGrant::placeholder`], D242), what
/// stands for its credential in a request, so that the run's injector fills a value in where
/// its own placeholder stands and nowhere else; 23, the run's microVM saved whole for its
/// owner ([`to_host::SAVE`], answered [`to_client::SAVED`]; D259), so that a side that saves
/// none refuses the run as it is asked, before any save is; 24, the run's sign-in grants
/// ([`Request::sign_ins`], D242 step 8) and their words ([`to_client::CAPTURE`],
/// [`to_client::REFRESH`] and [`to_client::REVOKE`], answered [`to_host::CAPTURED`] and
/// [`to_host::REFRESHED`]), so that a side that knows no sign-in refuses the run by its version; 25, each member's MCP
/// bridge ([`MemberStage::mcp`], D254), so that a side that makes none never starts a harness
/// whose MCP config names one that is not there; 26, the run's upstream roots
/// ([`Request::upstream_roots`], D242), so that a side that knows none never runs an injector
/// that trusts fewer roots than its orchestrator named.
pub const VERSION: u8 = 26;

/// What the orchestrator's side sends.
pub mod to_host {
    /// [`Request`](super::Request), first and once.
    pub const CREATE: u8 = 1;
    /// [`Credit`](super::Credit): room for more of a member's channel.
    pub const CREDIT: u8 = 2;
    /// A [`StatsAsk`](super::StatsAsk).
    pub const STATS: u8 = 4;
    /// Ends the run at once, its VM killed.
    pub const STOP: u8 = 5;
    /// A [`PlugAsk`](super::PlugAsk): a host directory plugged into a running member
    /// (D235).
    pub const PLUG: u8 = 6;
    /// An [`UnplugAsk`](super::UnplugAsk).
    pub const UNPLUG: u8 = 7;
    /// [`Input`](super::Input): bytes for a member's stdin, within the room its host gave.
    pub const INPUT: u8 = 8;
    /// [`InputEnd`](super::InputEnd): the orchestrator's end of a member's stdin, or its
    /// abort.
    pub const INPUT_END: u8 = 9;
    /// A [`RoomAsk`](super::RoomAsk): the room the orchestrator's budget leaves the run
    /// (D188, D241).
    pub const ROOM: u8 = 10;
    /// A [`ResizeAsk`](super::ResizeAsk): a member's own terminal sized (D244).
    pub const RESIZE: u8 = 11;
    /// A [`RateAsk`](super::RateAsk): the CPU rate the orchestrator holds the run's vCPUs to
    /// (D191, D247).
    pub const RATE: u8 = 12;
    /// A [`LeaseAnswer`](super::LeaseAnswer): a lease its run host asked, given or refused,
    /// by its ask's id (D242).
    pub const LEASED: u8 = 13;
    /// A [`Request`](super::Request), first and once, in place of `CREATE`: a warm run host's
    /// exemplar ([`Request::exemplar`](super::Request::exemplar), D253's pools), its microVM
    /// started for runs of it and waiting, `WARMED`, for one, whose `CREATE` follows.
    pub const WARM: u8 = 14;
    /// A [`SaveAsk`](super::SaveAsk): the run's microVM saved whole into a directory its
    /// owner names (D259).
    pub const SAVE: u8 = 15;
    /// A [`CaptureAnswer`](super::CaptureAnswer): a sign-in's tokens kept and their
    /// placeholders given, or why not, by its ask's id (D242, step 8).
    pub const CAPTURED: u8 = 34;
    /// A [`RefreshAnswer`](super::RefreshAnswer): the token response a sign-in's CLI is to
    /// have, refreshed outside, or why not, by its ask's id.
    pub const REFRESHED: u8 = 35;
}

/// What the run host sends.
pub mod to_client {
    /// [`Started`](super::Started): the run runs, its members started.
    pub const STARTED: u8 = 16;
    /// [`Refused`](super::Refused): the run will not be, and the host ends.
    pub const REFUSED: u8 = 17;
    /// [`Output`](super::Output): a member's bytes, within its credit.
    pub const OUTPUT: u8 = 18;
    /// [`OutputEnd`](super::OutputEnd): a member's channel has ended.
    pub const OUTPUT_END: u8 = 19;
    /// [`MemberEnded`](super::MemberEnded): a member's population has ended.
    pub const MEMBER_ENDED: u8 = 20;
    /// [`StatsAnswer`](super::StatsAnswer).
    pub const STATS: u8 = 21;
    /// [`RunEnd`](super::RunEnd), last: the run has ended.
    pub const ENDED: u8 = 22;
    /// [`Ready`](super::Ready): the run's own command moved its readiness under the nesting
    /// grant (D234), as its init said it ([`ready`](super::ready) reads it).
    pub const READY: u8 = 23;
    /// [`PlugAnswer`](super::PlugAnswer): a plug's or an unplug's outcome, by its ask's id.
    pub const PLUGGED: u8 = 24;
    /// [`InputRoom`](super::InputRoom): room for more of a member's stdin.
    pub const INPUT_ROOM: u8 = 25;
    /// [`InputEnded`](super::InputEnded): a member's stdin has ended, and how.
    pub const INPUT_ENDED: u8 = 26;
    /// [`RoomAnswer`](super::RoomAnswer): what the run is held within, by its ask's id.
    pub const HELD: u8 = 27;
    /// [`ResizeAnswer`](super::ResizeAnswer): whether a member's terminal took its size, by
    /// its ask's id.
    pub const RESIZED: u8 = 28;
    /// [`RateAnswer`](super::RateAnswer): what the run's vCPUs are held to, by its ask's id.
    pub const RATED: u8 = 29;
    /// A [`LeaseAsk`](super::LeaseAsk): a credential's lease, asked just in time by reference
    /// (D242).
    pub const LEASE: u8 = 30;
    /// Every lease of the run released ([`released`](super::released)): the run has ended,
    /// and nothing of any value it was leased is held (D242). Before [`ENDED`].
    pub const RELEASED: u8 = 31;
    /// How a warm run host's microVM started ([`StartedFrom`](super::StartedFrom), its one
    /// byte): it is ready, and waits for its run's `CREATE` (D253's pools).
    pub const WARMED: u8 = 32;
    /// [`SaveAnswer`](super::SaveAnswer): a save's outcome, by its ask's id (D259).
    pub const SAVED: u8 = 33;
    /// A [`CaptureAsk`](super::CaptureAsk): a sign-in's tokens, as the run's injector took them
    /// from its CLI's token response, to keep outside (D242, step 8).
    pub const CAPTURE: u8 = 36;
    /// A [`RefreshAsk`](super::RefreshAsk): a sign-in's refresh, as its CLI asked it.
    pub const REFRESH: u8 = 37;
    /// [`Revoked`](super::Revoked): the run's VM revoked a sign-in, answered in its network
    /// process; told, unanswered.
    pub const REVOKE: u8 = 38;
}

/// Why a payload could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// It ends before a field does.
    Short,
    /// Bytes follow its last field.
    Trailing,
    /// A tag no version of this channel has.
    Tag { what: &'static str, tag: u8 },
    /// Text that is not UTF-8.
    Text,
    /// A version this side does not speak.
    Version(u8),
    /// A payload past `run::MAX_PAYLOAD`, or a count or length past what a u32 holds.
    TooLong,
    /// A list whose items come out of the order the channel keeps them in.
    Order { what: &'static str },
    /// A vCPU's unread cause not in its text form (`shards_abi::guest_time::Unread::write`).
    Unread,
    /// A GPU's identity not in D232's grammar, or a selection D232 refuses (`shards_abi::gpu`).
    Gpu,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Short => f.write_str("a payload ends before its fields"),
            Self::Trailing => f.write_str("bytes after a payload's last field"),
            Self::Tag { what, tag } => write!(f, "{what}: unknown tag {tag}"),
            Self::Text => f.write_str("text that is not UTF-8"),
            Self::Version(v) => write!(f, "channel version {v}, where this side speaks {VERSION}"),
            Self::TooLong => f.write_str("a payload past the channel's bound"),
            Self::Order { what } => write!(f, "{what} out of order"),
            Self::Unread => f.write_str("a vCPU's unread cause this side does not parse"),
            Self::Gpu => f.write_str("a GPU's identity D232's grammar or selection refuses"),
        }
    }
}

impl std::error::Error for WireError {}

/// Bytes a caller would not have printed: a credential, as often as not. Its `Debug` says
/// how many there are, never what, and its bytes are wiped as it drops (D242).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(pub Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Secret(s.as_bytes().to_vec())
    }
}

/// Which declaration a member is (AGENTFILE_ARCH.md §4.2, §4.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemberKind {
    Agent = 0,
    Harness = 1,
}

/// One of a member's two output channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Stdout = 1,
    Stderr = 2,
}

/// A published port's IP protocol, by its IANA number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp = 6,
    Udp = 17,
}

/// A file staged under a member's home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    /// Relative to the member's home; no `..`, no empty or `.` component.
    pub path: String,
    /// Permission bits alone (at most 0o777).
    pub mode: u32,
    pub bytes: Secret,
}

/// What the orchestrator gives one member at create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberStage {
    pub kind: MemberKind,
    pub name: String,
    /// `KEY=VALUE` each, over its image's.
    pub env: Vec<Secret>,
    pub files: Vec<StagedFile>,
    /// Its working directory, absolute and plain; none for its image's.
    pub workdir: Option<String>,
    /// Its stdin is the orchestrator's to write ([`Input`]); else it is `/dev/null`.
    pub input: bool,
    /// Its stdio is a terminal of its own (D244): what it shows is its stdout channel, its
    /// stdin goes to it, and [`ResizeAsk`] sizes it.
    pub terminal: bool,
    /// Its run-time arguments (D256), on its command line after its config's `run.command`
    /// (its entrypoint and options), in their order; none, its command alone.
    pub args: Vec<String>,
    /// An MCP bridge of its own (D254): a stdio server at `/run/shards/mcp` that its harness's
    /// MCP config names, each connection to which reaches the run's controller as a lane of
    /// its agent-control endpoint ([`Request::control`], which it needs); none, and it has
    /// neither the bridge nor its socket.
    pub mcp: bool,
}

/// A guest port published on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publish {
    pub guest_port: u16,
    pub proto: Proto,
    /// The host address to bind; none for every one.
    pub host: Option<IpAddr>,
    /// The host port; 0 for one the OS picks.
    pub host_port: u16,
}

/// What of a grant list the orchestrator keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    /// Everything the image grants.
    All,
    /// These alone, each one the image grants, as it writes them.
    Only(Vec<String>),
}

/// The image's grants past the microVM, as the orchestrator narrows them: never wider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Narrowing {
    /// Egress ports, written as the Agentfile writes them (`443/tcp`, `8000-8010/udp`).
    pub egress: Keep,
    /// Remote MCP servers, `host:port`.
    pub mcp: Keep,
    /// Destinations, `host:port` as a network's `--egress=HOST:PORT` names one (D236): a
    /// host's name, an IPv4 address, or `host.docker.internal`, the host itself.
    pub destinations: Keep,
    /// Names past the microVM, where the image grants them.
    pub dns: bool,
    /// Each member's reach of the names kept, where the orchestrator holds its members apart
    /// (version 12, D246): every member the image declares is held to its entry's, one named
    /// in none to nothing, or the run is refused before its microVM starts. A member reaches
    /// each destination and remote MCP server kept that its image lets it reach by name:
    /// D236 keeps them for the whole microVM, and its `held` makes members alike on a port
    /// they reach by name. None: no member held apart, the names kept every member's whose
    /// image names them.
    pub members: Option<Vec<MemberReach>>,
}

impl Default for Narrowing {
    fn default() -> Self {
        Narrowing {
            egress: Keep::All,
            mcp: Keep::All,
            destinations: Keep::All,
            dns: true,
            members: None,
        }
    }
}

/// One member's reach of the names kept ([`Narrowing::members`], D246): the member, by the
/// kind and name its image's Agentfile declares, and each destination or remote MCP server
/// it may reach, `host:port` as the narrowing keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberReach {
    pub kind: MemberKind,
    pub name: String,
    pub destinations: Vec<String>,
}

/// A run's credential grants and the injector's decision with them (D242): shards-dns's, which
/// the network process's injector decides with too (both name a grant's hosts by its rules).
/// A grant rides this channel by reference with the hosts it may be filled in toward and the
/// placeholder that stands for it; its value rides a lease the injector pulls just in time,
/// never this channel. The orchestrator's side checks it where it builds the request
/// ([`super::request`]).
pub use shards_dns::credential::{CredentialGrant, Fill, decide};

/// A credential's lease, asked by the run's host just in time (D242): the credential its
/// network process first needs, by reference, one of the run's grants and never a value;
/// answered by its id ([`LeaseAnswer`]). It names the run by the channel it comes on, which
/// the orchestrator that started the run's host alone speaks on (D231, D248).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseAsk {
    pub id: u32,
    pub reference: String,
}

/// A lease given (D242): the credential's value, and how long its lease lives past its
/// answer, in nanoseconds, never none. Its holder wipes the value as it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub value: Secret,
    pub lives: NonZeroU64,
}

/// Why no lease was given (D242), typed (R04): the run's host relays it to the network
/// process that asked, whose asker matches it, never its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseRefusal {
    /// The orchestrator's broker leases the run none of it, in its words: a credential its
    /// store does not hold, or one its policy keeps from the run.
    Denied(String),
    /// Asked under the key the run had before a move replaced it (D238, D176 part 6, as the
    /// daemon's `move step key` refuses one): that key, and the run's key now where it has
    /// one, each 64 lowercase hex digits. The broker alone says which key is current.
    KeyReplaced { asked: String, current: Option<String> },
    /// Its broker could not be asked, or failed, in its words: a later ask may be leased.
    Unavailable(String),
    /// The run has ended: nothing is leased after its release.
    Ended,
    /// Its lease ended before it was answered: a later ask is leased anew.
    Lapsed,
}

/// A lease's answer, by its ask's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseAnswer {
    pub id: u32,
    pub result: Result<Lease, LeaseRefusal>,
}

/// A run's sign-in grant and what its capture gave (D242, step 8): shards-dns', which the
/// network process holds too.
pub use shards_dns::credential::{Captured, Cli, SignInGrant};

/// A sign-in's tokens as its run's injector took them (D242, step 8), each wiped as it drops:
/// its access token, its refresh token where its response had one, and the access token's
/// life in seconds where its response said (RFC 6749 §5.1); and, no credential, what its CLI's
/// own file records beside them, for its orchestrator to stage into a later run's: its ID token
/// made inert as its CLI was given it (its header and claims, no signature: Codex's `auth.json`
/// parses it for its account), and the scope its access token was granted (Claude Code's
/// `scopes`). The real ID token stays nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInTokens {
    pub access: Secret,
    pub refresh: Option<Secret>,
    pub expires_in: Option<u64>,
    pub id_token: Option<Secret>,
    pub scope: Option<String>,
}

/// A sign-in's capture, asked by the run's host (D242, step 8): keep its tokens, and give the
/// placeholders its CLI is to hold in their places; answered by its id ([`CaptureAnswer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureAsk {
    pub id: u32,
    pub sign_in: String,
    pub tokens: SignInTokens,
}

/// A sign-in's refresh, asked by the run's host as its CLI asked it (D242, step 8): refreshed
/// by its broker once for every run of it at a time, answered by its id ([`RefreshAnswer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshAsk {
    pub id: u32,
    pub sign_in: String,
}

/// A sign-in the run's VM revoked (its CLI's logout, or a sign-in anew), answered in its
/// network process and its stored tokens kept (D242, step 8): told, unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    pub sign_in: String,
}

/// Why a sign-in's ask was not answered (D242, step 8), typed (R04): the run's host relays it
/// to its network process, whose injector matches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInRefusal {
    /// The orchestrator's broker refused it, in its words: a sign-in its store does not hold, or
    /// a refresh its provider refused.
    Denied(String),
    /// Its broker could not be asked, or failed, in its words: a later ask may be answered.
    Unavailable(String),
    /// The run has ended.
    Ended,
    /// The run's orchestrator answers no sign-in.
    Unanswered,
}

/// A capture's answer, by its ask's id: the placeholders the sign-in's CLI is to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureAnswer {
    pub id: u32,
    pub result: Result<Captured, SignInRefusal>,
}

/// A refresh's answer, by its ask's id: the token response the sign-in's CLI is to have, its
/// tokens the placeholders it holds (the run's injector gives them those whatever comes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshAnswer {
    pub id: u32,
    pub result: Result<Secret, SignInRefusal>,
}

/// A host directory the guest sees at a path, as `-v HOST:PATH[:ro]` gives one (D109).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareAt {
    /// Absolute, UTF-8.
    pub host: String,
    /// Where the guest sees it: absolute.
    pub at: String,
    pub read_only: bool,
}

/// The run an orchestrator asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The image, as `shards run` names one in its home's store.
    pub image: String,
    /// Its vCPUs; 0 as `shards run` sizes it.
    pub vcpus: u32,
    /// Its memory in MiB; 0 as `shards run` sizes it.
    pub memory_mib: u64,
    /// The run's own command; none for the image's members alone (`builtin::ATTEND`).
    pub command: Option<Vec<String>>,
    pub members: Vec<MemberStage>,
    pub publish: Vec<Publish>,
    pub egress: Narrowing,
    pub shares: Vec<ShareAt>,
    /// The nesting grant for the run's own command (D234), as `--security-opt nesting=true`
    /// asks for it.
    pub nesting: bool,
    /// The grant's deadline (D234): a budget in nanoseconds from when the run host reads
    /// this request, which it makes an instant on its own clock; none for none.
    pub ready_by: Option<u64>,
    /// The run's credential grants (D242): each a credential named by reference and the
    /// hosts the injector may fill it in toward. Empty for a run with none, as every run
    /// is until an orchestrator asks for one (default deny).
    pub credentials: Vec<CredentialGrant>,
    /// The run's agent-control endpoint (D252), bound by its host for this run alone; none,
    /// and it has none. Appended in version 15.
    pub control: Option<ControlAsk>,
    /// The host GPUs its VM renders on, each by its exact identity (D232), which its command
    /// reads under the nesting grant (D234's `gpus=`); none, no GPU. Appended in version 16.
    pub gpu_devices: Vec<shards_abi::gpu::PhysicalGpu>,
    /// The run's sign-in grants (D242, step 8): each a sign-in its CLI may sign in as, its
    /// tokens kept outside and its CLI given placeholders. Empty for a run with none. Appended
    /// in version 24.
    pub sign_ins: Vec<SignInGrant>,
    /// The roots the run's injector trusts beside the host's own, each a certificate's DER
    /// (D242), for its injected flows alone; empty for the host's alone. Appended in version 26.
    pub upstream_roots: Vec<Vec<u8>>,
}

/// What a run's agent-control endpoint takes and serves (D252): the one key it takes, its
/// controller's, and the lanes to the run's own command's sockets (D251).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlAsk {
    /// The SHA-256 of the controller's key (D180's raw public key, RFC 7250): the one key the
    /// endpoint takes.
    pub controller: [u8; 32],
    /// The run's socket directory and what it admits of lanes to it; none, and none.
    pub sockets: Option<SocketGrant>,
}

/// The one directory of the run's own command's sockets its controller may reach (D251), and
/// what the run admits of lanes to them (R03: the caller's numbers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketGrant {
    /// Absolute and plain, in the command's root (`shards_abi::run::socket::dir_ok`).
    pub dir: String,
    /// The most lanes open at once.
    pub lanes: u32,
    /// Each lane's bytes in flight each way.
    pub window: u32,
}

/// Where a run's agent-control endpoint is bound, and the SHA-256 of its microVM's own key,
/// which its controller pins (D252).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlAt {
    pub address: SocketAddr,
    pub vm: [u8; 32],
}

/// Room for `bytes` more of member `member`'s `channel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credit {
    pub member: u32,
    pub channel: Channel,
    pub bytes: u64,
}

/// Which cgroup a stats ask reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsScope {
    /// The run's: its own command and every member.
    Run,
    /// The run's own command's.
    Command,
    /// One member's, by index.
    Member(u32),
}

/// A stats ask, its answer named by `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsAsk {
    pub id: u32,
    pub scope: StatsScope,
}

/// A member as the run host found it in the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    /// Its declared index: agents, then harnesses, as the Agentfile declares them.
    pub index: u32,
    pub kind: MemberKind,
    pub name: String,
    /// It has a program, which the run started; else it is files alone.
    pub runs: bool,
}

/// A published port, bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub guest_port: u16,
    pub proto: Proto,
    pub host: SocketAddr,
    /// Whom it reaches (D122): each member the image gives this port coming in, several
    /// alike where several claim it (the one listening takes each connection); none where
    /// no member claims it, and it reaches the run's own command.
    pub to: Vec<Receiving>,
}

/// A member a published port reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receiving {
    /// Its declared index, as [`MemberInfo::index`] says it.
    pub member: u32,
    /// The member's own port there: the guest port, unless an `EXPOSE` maps it to another
    /// (`<port>:<member_port>`, D122).
    pub port: u16,
}

/// The run runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub members: Vec<MemberInfo>,
    pub published: Vec<Bound>,
    /// Where the run's endpoint is bound, where it asked for one (D252); appended in version
    /// 15.
    pub control: Option<ControlAt>,
    /// The period its VM's governor holds a rate over (D191, D247), as its host's build
    /// holds one: on the channel in nanoseconds, never none; appended in
    /// version 17.
    pub rate_period: std::time::Duration,
    /// How its microVM started (D253); appended in version 19.
    pub from: StartedFrom,
}

/// How a run's microVM came to be ready (D253), as its run host started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartedFrom {
    /// Booted, with no template to restore or save: a guest its caller gave, or a host whose
    /// hypervisor restores no snapshot.
    Booted = 0,
    /// Booted, saving its image's template on the way, none being saved.
    Saved = 1,
    /// Restored from its image's template.
    Restored = 2,
    /// Booted, saving another, its image's template having failed to restore: that one
    /// removed.
    Replaced = 3,
    /// A warm run host's, its microVM ready before its run asked (D253's pools); appended in
    /// version 21.
    Warm = 4,
}

/// Why the run host will not run it, typed by what failed: the client maps each to its
/// [`Refusal`](crate::Refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub cause: Cause,
    pub why: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The request itself (a malformed field, a bound passed).
    Request = 0,
    /// The image, its Agentfile, or a member it names.
    Image = 1,
    /// A grant the image does not give, asked for.
    Widening = 2,
    /// A port, a share, a network or a VM that could not be made.
    Boot = 3,
    /// What the run host does not do yet.
    Unsupported = 4,
    /// A warm run host's microVM is another request's than the run's (D253's pools): the run
    /// is its orchestrator's to start on a host of its own.
    Unfit = 5,
}

/// A member's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub member: u32,
    pub channel: Channel,
    pub bytes: Vec<u8>,
}

/// Bytes for a member's stdin, at most the room its host has given and the orchestrator has
/// not used: what a person typed into the member, a credential as often as not, so its bytes
/// are a [`Secret`]'s, wiped as they drop and never printed (D242).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub member: u32,
    pub bytes: Secret,
}

/// The orchestrator's end of a member's stdin ([`StreamEnd::Closed`]), every byte before it
/// to go to the member, or its abort, with its errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEnd {
    pub member: u32,
    pub end: StreamEnd,
}

/// Room for `bytes` more of member `member`'s stdin: given once its stdin is ready, then as
/// each part the orchestrator sent is in the member's input connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRoom {
    pub member: u32,
    pub bytes: u64,
}

/// A member's stdin has ended: the bytes it took, and how, as its init told it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEnded {
    pub member: u32,
    pub written: u64,
    pub end: StreamEnd,
}

/// How a member's channel ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// Its writers all closed it.
    Closed,
    /// Its capture failed, with this errno.
    Aborted(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputEnd {
    pub member: u32,
    pub channel: Channel,
    pub end: StreamEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberEnded {
    pub member: u32,
    /// As `docker run` reports one: its code, or 128 plus a fatal signal's number.
    pub status: u32,
}

/// What init's `STATS` says, each line `None` where it said none; and, for the run, what its
/// microVM costs its host beside it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stats {
    /// CPU time, in ns.
    pub cpu_ns: Option<u64>,
    /// The whole guest's CPU time, in ns, and its CPUs online.
    pub system_ns: Option<u64>,
    pub online_cpus: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub inactive_file_bytes: Option<u64>,
    pub limit_bytes: Option<u64>,
    pub pids: Option<u64>,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    /// The run's alone ([`StatsScope::Run`]), as its run host reads it: none for its command
    /// or a member, whose cgroups are inside the microVM it costs.
    pub microvm: Option<Microvm>,
}

/// What a run's microVM costs its host, which no cgroup of its guest holds, as a daemon's
/// stats say it of its own runs (D43, D170's `microvm`; guest-cpu-time-v1): its VM process's
/// use of the host, and what each of its vCPUs has run of its guest, read before that use,
/// so that the process's CPU time less its vCPUs' is its VMM's own and never less than none
/// past the clocks' steps (D241). Each part is its reading, or why its run host has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Microvm {
    pub host: Result<HostUse, String>,
    pub guest: Result<GuestTimes, String>,
}

/// What a VM process uses of its host, and what a microVM's vCPUs have run of its guest, each
/// by index, with what those it no longer has ran (guest-cpu-time-v1): one form for the VM
/// process, an image run's stats and a managed guest's use (D260).
pub use shards_abi::guest_time::{GuestTimes, HostUse, VcpuTime};

impl Microvm {
    /// Its VMM's own CPU time, in ns: its VM process's less what its vCPUs ran of its guest,
    /// none less than none past the clocks' steps (D241, D260); none where either was not read.
    pub fn vmm_ns(&self) -> Option<u64> {
        let host = self.host.as_ref().ok()?;
        let guest = self.guest.as_ref().ok()?;
        Some(host.cpu_ns.saturating_sub(guest.total_ns()))
    }
}

impl Stats {
    /// Init's `KEY VALUE` lines (init run.rs `stats`): each known key's decimal value; an
    /// unknown key is passed over, a known one with no number is no value.
    pub fn parse(text: &str) -> Stats {
        let mut s = Stats::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            let value = value.trim().parse::<u64>().ok();
            match key {
                "cpu" => s.cpu_ns = value,
                "system" => s.system_ns = value,
                "online" => s.online_cpus = value,
                "memory" => s.memory_bytes = value,
                "inactive_file" => s.inactive_file_bytes = value,
                "limit" => s.limit_bytes = value,
                "pids" => s.pids = value,
                "read" => s.read_bytes = value,
                "write" => s.write_bytes = value,
                "rx" => s.rx_bytes = value,
                "tx" => s.tx_bytes = value,
                _ => {}
            }
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsAnswer {
    pub id: u32,
    /// Init's lines, or why none came.
    pub result: Result<Stats, String>,
}

/// The room the orchestrator's budget leaves the run (D188, D241), as a daemon's budget
/// tells a run of its own: the bytes it may use past what it uses, negative where the
/// budget is short of what it holds; none where no budget holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomAsk {
    pub id: u32,
    pub room: Option<i64>,
}

/// What the run's init held it within past what it uses, by its ask's id: the bytes, none
/// where its ask said no budget holds it, or why it held it within nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomAnswer {
    pub id: u32,
    pub result: Result<Option<u64>, String>,
}

/// A member's own terminal sized (D244), as TIOCSWINSZ sets one: its rows and columns, a 0
/// in either leaving it as it is, as an exec's is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeAsk {
    pub id: u32,
    pub member: u32,
    pub rows: u16,
    pub cols: u16,
}

/// Whether a member's terminal took its size, by its ask's id, or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeAnswer {
    pub id: u32,
    pub result: Result<(), String>,
}

/// The CPU rate the orchestrator holds the run's vCPUs to together (D191, D247), in
/// milli-CPU of the host's time; none for no hold, the microVM's vCPUs its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateAsk {
    pub id: u32,
    pub rate: Option<NonZeroU64>,
}

/// What the run's VM holds its vCPUs to, by its ask's id: the milli-CPU, none where it lets
/// them be (no rate, or every vCPU's whole CPU or more), or why it holds them to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateAnswer {
    pub id: u32,
    pub result: Result<Option<NonZeroU64>, String>,
}

/// A host directory plugged into running member `member` as the run goes (D235), as
/// `shards plug volume --for` plugs one: at `at`, absolute and strictly beneath one of the
/// run's plug roots, which its host names as the run starts (its OS's own, and the
/// top-level directory of each of its [`ShareAt`]s), so that a project can be plugged at
/// its host path; the host's files there its member's, read-only where asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlugAsk {
    pub id: u32,
    pub member: u32,
    /// Absolute, UTF-8: the host's directory.
    pub host: String,
    /// Absolute: where its member sees it.
    pub at: String,
    pub read_only: bool,
}

/// What is plugged in at `at` let go of: unmounted in its member, refused while anything
/// there uses it, then taken off the microVM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnplugAsk {
    pub id: u32,
    pub at: String,
}

/// Why a plug or an unplug was refused, by what refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlugRefusal {
    /// The ask itself: a path that is not absolute, a member the image does not declare.
    Request = 0,
    /// The member runs nothing: files alone, or ended.
    Member = 1,
    /// `at` lies beneath none of the run's plug roots, which its words name.
    Outside = 2,
    /// Something is plugged in at `at` already.
    Taken = 3,
    /// The host's directory could not be opened, or served.
    Host = 4,
    /// The microVM could not take it: no slot free, or its guest refused it.
    Device = 5,
    /// An unplug's: nothing is plugged in at `at`.
    Unknown = 6,
    /// An unplug's: something in the member uses it.
    Busy = 7,
}

fn plug_refusal(tag: u8) -> Result<PlugRefusal, WireError> {
    Ok(match tag {
        0 => PlugRefusal::Request,
        1 => PlugRefusal::Member,
        2 => PlugRefusal::Outside,
        3 => PlugRefusal::Taken,
        4 => PlugRefusal::Host,
        5 => PlugRefusal::Device,
        6 => PlugRefusal::Unknown,
        7 => PlugRefusal::Busy,
        tag => {
            return Err(WireError::Tag {
                what: "a plug's refusal",
                tag,
            });
        }
    })
}

/// A plug's or an unplug's outcome, by its ask's id: done, or refused and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlugAnswer {
    pub id: u32,
    pub result: Result<(), (PlugRefusal, String)>,
}

/// The run's microVM saved whole into directory `to`, which the save makes (D259): absolute,
/// beneath the run's home, beneath no directory its guest reaches, and nothing there yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveAsk {
    pub id: u32,
    /// Absolute, UTF-8: the directory the save makes.
    pub to: String,
}

/// Why a save was refused, by what refused it (D259).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveRefusal {
    /// Where it was to be written is not its owner's to name: not absolute, outside the run's
    /// home, beneath a directory its guest reaches, or something there already.
    Destination = 0,
    /// The run is in no state to save: a save of it under way, or its microVM gone.
    State = 1,
    /// This host's microVMs take no snapshots (`shards_vmm::vm::SNAPSHOTS`).
    Unsupported = 2,
    /// It was not made, in the words of what failed: its VM's (what its snapshot cannot keep
    /// whole, its guest not going quiet) or its files' writing.
    Failed = 3,
}

fn save_refusal(tag: u8) -> Result<SaveRefusal, WireError> {
    Ok(match tag {
        0 => SaveRefusal::Destination,
        1 => SaveRefusal::State,
        2 => SaveRefusal::Unsupported,
        3 => SaveRefusal::Failed,
        tag => {
            return Err(WireError::Tag {
                what: "a save's refusal",
                tag,
            });
        }
    })
}

/// A save's outcome, by its ask's id: saved, or refused and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveAnswer {
    pub id: u32,
    pub result: Result<(), (SaveRefusal, String)>,
}

/// How the run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// Its own command, or `ATTEND`'s holder, ended with this status.
    Exited(u32),
    /// `STOP`: its VM killed at once.
    Stopped,
    /// Its VM, its host or its guest failed, or its own command never started, in its VM's
    /// words (D249).
    Failed(String),
}

/// A frame's header for `kind` and a payload of `len` bytes, refused past the bound.
pub fn header(kind: u8, len: usize) -> Result<[u8; run::HEADER], WireError> {
    let len = u32::try_from(len).map_err(|_| WireError::TooLong)?;
    if len > run::MAX_PAYLOAD {
        return Err(WireError::TooLong);
    }
    Ok(run::header(kind, len))
}

/// A frame: its header and payload, one buffer.
pub fn frame(kind: u8, payload: &[u8]) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    out.extend_from_slice(&header(kind, payload.len())?);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Reads one frame from `from`: its kind and payload, none at a clean end before a header,
/// an error past the bound or cut short.
pub fn read_frame(from: &mut impl std::io::Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut payload = Vec::new();
    Ok(read_frame_into(from, &mut payload)?.map(|(kind, _)| (kind, payload)))
}

/// Reads one frame from `from` as [`read_frame`] does, its payload into `buf[..length]`:
/// its kind and length. `buf` is grown with zeros where it is shorter, and never shrunk or
/// read past, so that a reader that wipes each payload as it lets it go of holds no frame's
/// bytes between frames (the run host's channel, D242), and reads nothing past its frame.
pub fn read_frame_into(
    from: &mut impl std::io::Read,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<(u8, usize)>> {
    let mut h = [0u8; run::HEADER];
    let mut got = 0;
    while got < h.len() {
        let n = from.read(h.get_mut(got..).unwrap_or_default())?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(std::io::ErrorKind::UnexpectedEof.into())
            };
        }
        got += n;
    }
    let (kind, len) = run::parse_header(h)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, WireError::TooLong))?;
    let len = usize::try_from(len)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, WireError::TooLong))?;
    if let Some(more) = len.checked_sub(buf.len()).filter(|more| *more > 0) {
        buf.try_reserve_exact(more)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::OutOfMemory, "a frame's payload"))?;
        buf.resize(len, 0);
    }
    let payload = buf
        .get_mut(..len)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::OutOfMemory, "a frame's payload"))?;
    from.read_exact(payload)?;
    Ok(Some((kind, len)))
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn count(&mut self, n: usize) -> Result<(), WireError> {
        self.u32(u32::try_from(n).map_err(|_| WireError::TooLong)?);
        Ok(())
    }
    fn bytes(&mut self, b: &[u8]) -> Result<(), WireError> {
        self.count(b.len())?;
        self.0.extend_from_slice(b);
        Ok(())
    }
    fn text(&mut self, s: &str) -> Result<(), WireError> {
        self.bytes(s.as_bytes())
    }
    /// A capture's access reference and its placeholder, then 1 and its refresh token's
    /// placeholder where it has one, else 0 (D242, step 8).
    fn captured(&mut self, c: &Captured) -> Result<(), WireError> {
        self.text(&c.access)?;
        self.text(&c.access_placeholder)?;
        match &c.refresh_placeholder {
            None => self.u8(0),
            Some(refresh) => {
                self.u8(1);
                self.text(refresh)?;
            }
        }
        Ok(())
    }
    fn ip(&mut self, ip: Option<IpAddr>) {
        match ip {
            None => self.u8(0),
            Some(IpAddr::V4(a)) => {
                self.u8(4);
                self.0.extend_from_slice(&a.octets());
            }
            Some(IpAddr::V6(a)) => {
                self.u8(6);
                self.0.extend_from_slice(&a.octets());
            }
        }
    }
    fn keep(&mut self, k: &Keep) -> Result<(), WireError> {
        match k {
            Keep::All => self.u8(0),
            Keep::Only(items) => {
                self.u8(1);
                self.count(items.len())?;
                for i in items {
                    self.text(i)?;
                }
            }
        }
        Ok(())
    }
    fn stream_end(&mut self, end: StreamEnd) {
        match end {
            StreamEnd::Closed => self.u8(0),
            StreamEnd::Aborted(errno) => {
                self.u8(1);
                self.i32(errno);
            }
        }
    }
    fn microvm(&mut self, microvm: Option<&Microvm>) -> Result<(), WireError> {
        let Some(m) = microvm else {
            self.u8(0);
            return Ok(());
        };
        self.u8(1);
        match &m.host {
            Ok(h) => {
                self.u8(0);
                self.u64(h.cpu_ns);
                self.u64(h.resident_bytes);
                self.u64(h.footprint_bytes);
            }
            Err(why) => {
                self.u8(1);
                self.text(why)?;
            }
        }
        let g = match &m.guest {
            Ok(g) => g,
            Err(why) => {
                self.u8(1);
                return self.text(why);
            }
        };
        self.u8(0);
        self.u64(g.departed_ns);
        self.count(g.vcpus.len())?;
        for v in &g.vcpus {
            self.u32(v.index);
            self.u64(v.guest_ns);
            match &v.unread {
                None => self.u8(0),
                Some(why) => {
                    self.u8(1);
                    let mut text = String::new();
                    let _ = why.write(&mut text);
                    self.text(&text)?;
                }
            }
        }
        Ok(())
    }
    fn done(self) -> Result<Vec<u8>, WireError> {
        if self.0.len() > run::MAX_PAYLOAD as usize {
            return Err(WireError::TooLong);
        }
        Ok(self.0)
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// What [`Writer::captured`] wrote.
    fn captured(&mut self) -> Result<Captured, WireError> {
        Ok(Captured {
            access: self.text()?,
            access_placeholder: self.text()?,
            refresh_placeholder: if self.flag()? { Some(self.text()?) } else { None },
        })
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let data: &'a [u8] = self.0;
        let (head, rest) = data.split_at_checked(n).ok_or(WireError::Short)?;
        self.0 = rest;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        self.take(N)?.try_into().map_err(|_| WireError::Short)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(u8::from_be_bytes(self.array()?))
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_be_bytes(self.array()?))
    }
    fn i64(&mut self) -> Result<i64, WireError> {
        Ok(i64::from_be_bytes(self.array()?))
    }
    fn flag(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(WireError::Tag { what: "a flag", tag }),
        }
    }
    /// A count no larger than the bytes left could hold, each item at least `least` bytes:
    /// a hostile count allocates nothing it cannot fill.
    fn count(&mut self, least: usize) -> Result<usize, WireError> {
        let n = usize::try_from(self.u32()?).map_err(|_| WireError::TooLong)?;
        if n.checked_mul(least.max(1)).is_none_or(|need| need > self.0.len()) {
            return Err(WireError::Short);
        }
        Ok(n)
    }
    fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let n = usize::try_from(self.u32()?).map_err(|_| WireError::TooLong)?;
        self.take(n)
    }
    fn text(&mut self) -> Result<String, WireError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| WireError::Text)
    }
    fn ip(&mut self) -> Result<Option<IpAddr>, WireError> {
        match self.u8()? {
            0 => Ok(None),
            4 => Ok(Some(IpAddr::from(self.array::<4>()?))),
            6 => Ok(Some(IpAddr::from(self.array::<16>()?))),
            tag => Err(WireError::Tag {
                what: "an address",
                tag,
            }),
        }
    }
    fn keep(&mut self) -> Result<Keep, WireError> {
        match self.u8()? {
            0 => Ok(Keep::All),
            1 => {
                let n = self.count(4)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.text()?);
                }
                Ok(Keep::Only(items))
            }
            tag => Err(WireError::Tag {
                what: "a grant list",
                tag,
            }),
        }
    }
    fn stream_end(&mut self, what: &'static str) -> Result<StreamEnd, WireError> {
        match self.u8()? {
            0 => Ok(StreamEnd::Closed),
            1 => Ok(StreamEnd::Aborted(self.i32()?)),
            tag => Err(WireError::Tag { what, tag }),
        }
    }
    fn microvm(&mut self) -> Result<Option<Microvm>, WireError> {
        match self.u8()? {
            0 => return Ok(None),
            1 => {}
            tag => {
                return Err(WireError::Tag {
                    what: "a microVM's cost",
                    tag,
                });
            }
        }
        let host = match self.u8()? {
            0 => Ok(HostUse {
                cpu_ns: self.u64()?,
                resident_bytes: self.u64()?,
                footprint_bytes: self.u64()?,
            }),
            1 => Err(self.text()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a VM process's use",
                    tag,
                });
            }
        };
        let guest = match self.u8()? {
            0 => {
                let departed_ns = self.u64()?;
                // Each vCPU at least its index, its time and its flag.
                let n = self.count(4 + 8 + 1)?;
                let mut vcpus: Vec<VcpuTime> = Vec::with_capacity(n);
                for _ in 0..n {
                    let index = self.u32()?;
                    let guest_ns = self.u64()?;
                    let unread = match self.u8()? {
                        0 => None,
                        1 => Some(
                            shards_abi::guest_time::Unread::parse(&self.text()?).ok_or(WireError::Unread)?,
                        ),
                        tag => {
                            return Err(WireError::Tag {
                                what: "a vCPU's guest time",
                                tag,
                            });
                        }
                    };
                    if vcpus.last().is_some_and(|v| v.index >= index) {
                        return Err(WireError::Order {
                            what: "a microVM's vCPUs",
                        });
                    }
                    vcpus.push(VcpuTime {
                        index,
                        guest_ns,
                        unread,
                    });
                }
                Ok(GuestTimes { vcpus, departed_ns })
            }
            1 => Err(self.text()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a microVM's guest time",
                    tag,
                });
            }
        };
        Ok(Some(Microvm { host, guest }))
    }
    fn end(self) -> Result<(), WireError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(WireError::Trailing)
        }
    }
}

fn member_kind(tag: u8) -> Result<MemberKind, WireError> {
    match tag {
        0 => Ok(MemberKind::Agent),
        1 => Ok(MemberKind::Harness),
        tag => Err(WireError::Tag {
            what: "a member's kind",
            tag,
        }),
    }
}

fn channel(tag: u8) -> Result<Channel, WireError> {
    match tag {
        1 => Ok(Channel::Stdout),
        2 => Ok(Channel::Stderr),
        tag => Err(WireError::Tag {
            what: "a channel",
            tag,
        }),
    }
}

fn started_from(tag: u8) -> Result<StartedFrom, WireError> {
    match tag {
        0 => Ok(StartedFrom::Booted),
        1 => Ok(StartedFrom::Saved),
        2 => Ok(StartedFrom::Restored),
        3 => Ok(StartedFrom::Replaced),
        4 => Ok(StartedFrom::Warm),
        tag => Err(WireError::Tag {
            what: "how a run started",
            tag,
        }),
    }
}

fn proto(tag: u8) -> Result<Proto, WireError> {
    match tag {
        6 => Ok(Proto::Tcp),
        17 => Ok(Proto::Udp),
        tag => Err(WireError::Tag {
            what: "a protocol",
            tag,
        }),
    }
}

impl Request {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u8(VERSION);
        w.text(&self.image)?;
        w.u32(self.vcpus);
        w.u64(self.memory_mib);
        match &self.command {
            None => w.u8(0),
            Some(argv) => {
                w.u8(1);
                w.count(argv.len())?;
                for a in argv {
                    w.text(a)?;
                }
            }
        }
        w.count(self.members.len())?;
        for m in &self.members {
            w.u8(m.kind as u8);
            w.text(&m.name)?;
            w.count(m.env.len())?;
            for e in &m.env {
                w.bytes(&e.0)?;
            }
            w.count(m.files.len())?;
            for f in &m.files {
                w.text(&f.path)?;
                w.u32(f.mode);
                w.bytes(&f.bytes.0)?;
            }
        }
        w.count(self.publish.len())?;
        for p in &self.publish {
            w.u16(p.guest_port);
            w.u8(p.proto as u8);
            w.ip(p.host);
            w.u16(p.host_port);
        }
        w.keep(&self.egress.egress)?;
        w.keep(&self.egress.mcp)?;
        w.u8(u8::from(self.egress.dns));
        w.count(self.shares.len())?;
        for s in &self.shares {
            w.text(&s.host)?;
            w.text(&s.at)?;
            w.u8(u8::from(s.read_only));
        }
        w.u8(u8::from(self.nesting));
        match self.ready_by {
            None => w.u8(0),
            Some(budget) => {
                w.u8(1);
                w.u64(budget);
            }
        }
        w.keep(&self.egress.destinations)?;
        // Each member's working directory, in the members' order, appended in version 5.
        for m in &self.members {
            match &m.workdir {
                None => w.u8(0),
                Some(path) => {
                    w.u8(1);
                    w.text(path)?;
                }
            }
        }
        // Each member's input, in the members' order, appended in version 6.
        for m in &self.members {
            w.u8(u8::from(m.input));
        }
        // The run's credential grants, appended in version 8 (D242): each a reference and
        // the hosts it may be filled in toward.
        w.count(self.credentials.len())?;
        for grant in &self.credentials {
            w.text(&grant.reference)?;
            w.count(grant.toward.len())?;
            for host in &grant.toward {
                w.text(host)?;
            }
        }
        // Each member's terminal, in the members' order, appended in version 11 (D244).
        for m in &self.members {
            w.u8(u8::from(m.terminal));
        }
        // Each member's reach of the names kept, appended in version 12 (D246): whether the
        // members are held apart, then each member's kind, name and names.
        match &self.egress.members {
            None => w.u8(0),
            Some(members) => {
                w.u8(1);
                w.count(members.len())?;
                for m in members {
                    w.u8(m.kind as u8);
                    w.text(&m.name)?;
                    w.count(m.destinations.len())?;
                    for d in &m.destinations {
                        w.text(d)?;
                    }
                }
            }
        }
        // The run's agent-control endpoint, appended in version 15 (D252): whether it asks one,
        // its controller's key, then whether it grants sockets, their directory and admission.
        match &self.control {
            None => w.u8(0),
            Some(control) => {
                w.u8(1);
                w.0.extend_from_slice(&control.controller);
                match &control.sockets {
                    None => w.u8(0),
                    Some(grant) => {
                        w.u8(1);
                        w.text(&grant.dir)?;
                        w.u32(grant.lanes);
                        w.u32(grant.window);
                    }
                }
            }
        }
        // The GPUs the run names, appended in version 16 (D232): each in `--gpu-device`'s text.
        w.count(self.gpu_devices.len())?;
        for gpu in &self.gpu_devices {
            w.text(&gpu.to_string())?;
        }
        // Each member's run-time arguments, in the members' order, appended in version 18
        // (D256): its count, then each.
        for m in &self.members {
            w.count(m.args.len())?;
            for a in &m.args {
                w.text(a)?;
            }
        }
        // Each credential grant's placeholder, in the grants' order, appended in version 22
        // (D242).
        for grant in &self.credentials {
            w.text(&grant.placeholder)?;
        }
        // The run's sign-in grants, appended in version 24 (D242, step 8): each its name, its
        // CLI's code, its hosts, and whether a capture gave it placeholders, which follow.
        w.count(self.sign_ins.len())?;
        for s in &self.sign_ins {
            w.text(&s.sign_in)?;
            w.u8(s.cli.code());
            w.count(s.toward.len())?;
            for host in &s.toward {
                w.text(host)?;
            }
            match &s.captured {
                None => w.u8(0),
                Some(c) => {
                    w.u8(1);
                    w.captured(c)?;
                }
            }
        }
        // Each member's MCP bridge, in the members' order, appended in version 25 (D254).
        for m in &self.members {
            w.u8(u8::from(m.mcp));
        }
        // The run's upstream roots, appended in version 26 (D242): their count, then each DER.
        w.count(self.upstream_roots.len())?;
        for root in &self.upstream_roots {
            w.bytes(root)?;
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<Request, WireError> {
        let mut r = Reader(payload);
        let version = r.u8()?;
        if version != VERSION {
            return Err(WireError::Version(version));
        }
        let image = r.text()?;
        let vcpus = r.u32()?;
        let memory_mib = r.u64()?;
        let command = if r.flag()? {
            let n = r.count(4)?;
            let mut argv = Vec::with_capacity(n);
            for _ in 0..n {
                argv.push(r.text()?);
            }
            Some(argv)
        } else {
            None
        };
        // Each member's kind, name, env and files, and, appended last, its working
        // directory's flag, its input's, its terminal's, its arguments' count and its MCP
        // bridge's flag.
        let n = r.count(1 + 4 + 4 + 4 + 1 + 1 + 1 + 4 + 1)?;
        let mut members = Vec::with_capacity(n);
        for _ in 0..n {
            let kind = member_kind(r.u8()?)?;
            let name = r.text()?;
            let envs = r.count(4)?;
            let mut env = Vec::with_capacity(envs);
            for _ in 0..envs {
                env.push(Secret(r.bytes()?.to_vec()));
            }
            let nf = r.count(4 + 4 + 4)?;
            let mut files = Vec::with_capacity(nf);
            for _ in 0..nf {
                let path = r.text()?;
                let mode = r.u32()?;
                let bytes = Secret(r.bytes()?.to_vec());
                files.push(StagedFile { path, mode, bytes });
            }
            members.push(MemberStage {
                kind,
                name,
                env,
                files,
                // Read with the members' working directories, inputs, terminals, arguments and
                // MCP bridges, appended last.
                workdir: None,
                input: false,
                terminal: false,
                args: Vec::new(),
                mcp: false,
            });
        }
        let n = r.count(2 + 1 + 1 + 2)?;
        let mut publish = Vec::with_capacity(n);
        for _ in 0..n {
            let guest_port = r.u16()?;
            let proto = proto(r.u8()?)?;
            let host = r.ip()?;
            let host_port = r.u16()?;
            publish.push(Publish {
                guest_port,
                proto,
                host,
                host_port,
            });
        }
        // Its destinations come last, appended in version 3.
        let (egress, mcp, dns) = (r.keep()?, r.keep()?, r.flag()?);
        let n = r.count(4 + 4 + 1)?;
        let mut shares = Vec::with_capacity(n);
        for _ in 0..n {
            let host = r.text()?;
            let at = r.text()?;
            let read_only = r.flag()?;
            shares.push(ShareAt { host, at, read_only });
        }
        let nesting = r.flag()?;
        let ready_by = if r.flag()? { Some(r.u64()?) } else { None };
        let mut egress = Narrowing {
            egress,
            mcp,
            destinations: r.keep()?,
            dns,
            // Read with the members' reach, appended last.
            members: None,
        };
        for m in &mut members {
            m.workdir = if r.flag()? { Some(r.text()?) } else { None };
        }
        for m in &mut members {
            m.input = r.flag()?;
        }
        // The run's credential grants, appended in version 8 (D242): each grant is at least
        // its reference's length (four bytes) and its hosts' count (four), and each host at
        // least its length (four).
        let n = r.count(4 + 4)?;
        let mut credentials = Vec::with_capacity(n);
        for _ in 0..n {
            let reference = r.text()?;
            let toward_n = r.count(4)?;
            let mut toward = Vec::with_capacity(toward_n);
            for _ in 0..toward_n {
                toward.push(r.text()?);
            }
            // Its placeholder, read with the members' arguments, appended last.
            credentials.push(CredentialGrant {
                reference,
                toward,
                placeholder: String::new(),
            });
        }
        // Each member's terminal, in the members' order, appended in version 11 (D244).
        for m in &mut members {
            m.terminal = r.flag()?;
        }
        // Each member's reach of the names kept, appended in version 12 (D246): each member
        // is at least its kind (one byte), its name's length (four) and its names' count
        // (four), and each name at least its length (four).
        egress.members = if r.flag()? {
            let n = r.count(1 + 4 + 4)?;
            let mut reach = Vec::with_capacity(n);
            for _ in 0..n {
                let kind = member_kind(r.u8()?)?;
                let name = r.text()?;
                let k = r.count(4)?;
                let mut destinations = Vec::with_capacity(k);
                for _ in 0..k {
                    destinations.push(r.text()?);
                }
                reach.push(MemberReach {
                    kind,
                    name,
                    destinations,
                });
            }
            Some(reach)
        } else {
            None
        };
        // The run's agent-control endpoint, appended in version 15 (D252).
        let control = if r.flag()? {
            let controller = r.array::<32>()?;
            let sockets = if r.flag()? {
                Some(SocketGrant {
                    dir: r.text()?,
                    lanes: r.u32()?,
                    window: r.u32()?,
                })
            } else {
                None
            };
            Some(ControlAsk { controller, sockets })
        } else {
            None
        };
        // The GPUs the run names, appended in version 16 (D232): each at least its text's
        // length (four bytes), parsed and the selection checked as D232 checks one.
        let n = r.count(4)?;
        let mut gpu_devices = Vec::with_capacity(n);
        for _ in 0..n {
            gpu_devices.push(r.text()?.parse().map_err(|_| WireError::Gpu)?);
        }
        shards_abi::gpu::check_selection(&gpu_devices).map_err(|_| WireError::Gpu)?;
        // Each member's run-time arguments, appended in version 18 (D256): each at least its
        // text's length (four bytes).
        for m in &mut members {
            let n = r.count(4)?;
            let mut args = Vec::with_capacity(n);
            for _ in 0..n {
                args.push(r.text()?);
            }
            m.args = args;
        }
        // Each credential grant's placeholder, in the grants' order, appended in version 22
        // (D242).
        for grant in &mut credentials {
            grant.placeholder = r.text()?;
        }
        // The run's sign-in grants, appended in version 24 (D242, step 8): each at least its
        // name's length (four bytes), its CLI (one), its hosts' count (four) and its flag (one).
        let n = r.count(4 + 1 + 4 + 1)?;
        let mut sign_ins = Vec::with_capacity(n);
        for _ in 0..n {
            let sign_in = r.text()?;
            let code = r.u8()?;
            let cli = Cli::from_code(code).ok_or(WireError::Tag {
                what: "a sign-in's CLI",
                tag: code,
            })?;
            let k = r.count(4)?;
            let mut toward = Vec::with_capacity(k);
            for _ in 0..k {
                toward.push(r.text()?);
            }
            let captured = if r.flag()? { Some(r.captured()?) } else { None };
            sign_ins.push(SignInGrant {
                sign_in,
                cli,
                toward,
                captured,
            });
        }
        // Each member's MCP bridge, appended in version 25 (D254).
        for m in &mut members {
            m.mcp = r.flag()?;
        }
        // The run's upstream roots, appended in version 26 (D242): each at least its length's
        // four bytes.
        let n = r.count(4)?;
        let mut upstream_roots = Vec::with_capacity(n);
        for _ in 0..n {
            upstream_roots.push(r.bytes()?.to_vec());
        }
        r.end()?;
        Ok(Request {
            image,
            vcpus,
            memory_mib,
            command,
            members,
            publish,
            egress,
            shares,
            nesting,
            ready_by,
            credentials,
            control,
            gpu_devices,
            sign_ins,
            upstream_roots,
        })
    }
}

impl SignInRefusal {
    fn write(&self, w: &mut Writer) -> Result<(), WireError> {
        match self {
            SignInRefusal::Denied(why) => {
                w.u8(1);
                w.text(why)
            }
            SignInRefusal::Unavailable(why) => {
                w.u8(2);
                w.text(why)
            }
            SignInRefusal::Ended => {
                w.u8(3);
                Ok(())
            }
            SignInRefusal::Unanswered => {
                w.u8(4);
                Ok(())
            }
        }
    }

    fn read(r: &mut Reader<'_>) -> Result<SignInRefusal, WireError> {
        Ok(match r.u8()? {
            1 => SignInRefusal::Denied(r.text()?),
            2 => SignInRefusal::Unavailable(r.text()?),
            3 => SignInRefusal::Ended,
            4 => SignInRefusal::Unanswered,
            tag => {
                return Err(WireError::Tag {
                    what: "a sign-in's refusal",
                    tag,
                });
            }
        })
    }
}

impl CaptureAsk {
    /// Its id, its sign-in, the life and the scope where it has them (1 and each, else 0), then
    /// its tokens last, the refresh token's and the ID token's flags before them, in one buffer
    /// made to its length at once: no buffer let go of as one grows holds a part of a token. The
    /// caller wipes what it returns.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let t = &self.tokens;
        let sized = |v: Option<usize>| v.map_or(0, |n| n.saturating_add(4));
        let mut w = Writer(Vec::with_capacity(
            self.sign_in
                .len()
                .saturating_add(t.access.0.len())
                .saturating_add(sized(t.refresh.as_ref().map(|r| r.0.len())))
                .saturating_add(sized(t.id_token.as_ref().map(|i| i.0.len())))
                .saturating_add(sized(t.scope.as_ref().map(String::len)))
                .saturating_add(4 + 4 + 9 + 1 + 1 + 1 + 4),
        ));
        w.u32(self.id);
        w.text(&self.sign_in)?;
        match t.expires_in {
            None => w.u8(0),
            Some(s) => {
                w.u8(1);
                w.u64(s);
            }
        }
        match &t.scope {
            None => w.u8(0),
            Some(scope) => {
                w.u8(1);
                w.text(scope)?;
            }
        }
        w.u8(u8::from(t.refresh.is_some()));
        w.u8(u8::from(t.id_token.is_some()));
        w.bytes(&t.access.0)?;
        if let Some(r) = &t.refresh {
            w.bytes(&r.0)?;
        }
        if let Some(i) = &t.id_token {
            w.bytes(&i.0)?;
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<CaptureAsk, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let sign_in = r.text()?;
        let expires_in = if r.flag()? { Some(r.u64()?) } else { None };
        let scope = if r.flag()? { Some(r.text()?) } else { None };
        let (refreshes, identified) = (r.flag()?, r.flag()?);
        let access = Secret(r.bytes()?.to_vec());
        let refresh = if refreshes {
            Some(Secret(r.bytes()?.to_vec()))
        } else {
            None
        };
        let id_token = if identified {
            Some(Secret(r.bytes()?.to_vec()))
        } else {
            None
        };
        r.end()?;
        Ok(CaptureAsk {
            id,
            sign_in,
            tokens: SignInTokens {
                access,
                refresh,
                expires_in,
                id_token,
                scope,
            },
        })
    }
}

impl RefreshAsk {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        w.text(&self.sign_in)?;
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<RefreshAsk, WireError> {
        let mut r = Reader(payload);
        let ask = RefreshAsk {
            id: r.u32()?,
            sign_in: r.text()?,
        };
        r.end()?;
        Ok(ask)
    }
}

impl Revoked {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.text(&self.sign_in)?;
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<Revoked, WireError> {
        let mut r = Reader(payload);
        let told = Revoked { sign_in: r.text()? };
        r.end()?;
        Ok(told)
    }
}

impl CaptureAnswer {
    /// Its id, then 0 and the access token's reference and its placeholder, then 1 and the
    /// refresh token's where it has one, else 0; or 1 and its refusal.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match &self.result {
            Ok(c) => {
                w.u8(0);
                w.captured(c)?;
            }
            Err(refusal) => {
                w.u8(1);
                refusal.write(&mut w)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<CaptureAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => Ok(r.captured()?),
            1 => Err(SignInRefusal::read(&mut r)?),
            tag => {
                return Err(WireError::Tag {
                    what: "a capture's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(CaptureAnswer { id, result })
    }
}

impl RefreshAnswer {
    /// Its id, then 0 and the token response, last, in one buffer made to its length at once;
    /// or 1 and its refusal. The caller wipes what it returns.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let body = self.result.as_ref().map_or(0, |b| b.0.len());
        let mut w = Writer(Vec::with_capacity(body.saturating_add(4 + 1 + 4)));
        w.u32(self.id);
        match &self.result {
            Ok(body) => {
                w.u8(0);
                w.bytes(&body.0)?;
            }
            Err(refusal) => {
                w.u8(1);
                refusal.write(&mut w)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<RefreshAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => Ok(Secret(r.bytes()?.to_vec())),
            1 => Err(SignInRefusal::read(&mut r)?),
            tag => {
                return Err(WireError::Tag {
                    what: "a refresh's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(RefreshAnswer { id, result })
    }
}

impl Credit {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.member);
        w.u8(self.channel as u8);
        w.u64(self.bytes);
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<Credit, WireError> {
        let mut r = Reader(payload);
        let c = Credit {
            member: r.u32()?,
            channel: channel(r.u8()?)?,
            bytes: r.u64()?,
        };
        r.end()?;
        Ok(c)
    }
}

impl PlugAsk {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        w.u32(self.member);
        w.text(&self.host)?;
        w.text(&self.at)?;
        w.u8(u8::from(self.read_only));
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<PlugAsk, WireError> {
        let mut r = Reader(payload);
        let ask = PlugAsk {
            id: r.u32()?,
            member: r.u32()?,
            host: r.text()?,
            at: r.text()?,
            read_only: r.flag()?,
        };
        r.end()?;
        Ok(ask)
    }
}

impl UnplugAsk {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        w.text(&self.at)?;
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<UnplugAsk, WireError> {
        let mut r = Reader(payload);
        let ask = UnplugAsk {
            id: r.u32()?,
            at: r.text()?,
        };
        r.end()?;
        Ok(ask)
    }
}

impl PlugAnswer {
    /// Its id, then 0, or 1, its refusal's tag and why.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match &self.result {
            Ok(()) => w.u8(0),
            Err((cause, why)) => {
                w.u8(1);
                w.u8(*cause as u8);
                w.text(why)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<PlugAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => Ok(()),
            1 => Err((plug_refusal(r.u8()?)?, r.text()?)),
            tag => {
                return Err(WireError::Tag {
                    what: "a plug's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(PlugAnswer { id, result })
    }
}

impl SaveAsk {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        w.text(&self.to)?;
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<SaveAsk, WireError> {
        let mut r = Reader(payload);
        let ask = SaveAsk {
            id: r.u32()?,
            to: r.text()?,
        };
        r.end()?;
        Ok(ask)
    }
}

impl SaveAnswer {
    /// Its id, then 0, or 1, its refusal's tag and why.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match &self.result {
            Ok(()) => w.u8(0),
            Err((cause, why)) => {
                w.u8(1);
                w.u8(*cause as u8);
                w.text(why)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<SaveAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => Ok(()),
            1 => Err((save_refusal(r.u8()?)?, r.text()?)),
            tag => {
                return Err(WireError::Tag {
                    what: "a save's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(SaveAnswer { id, result })
    }
}

impl StatsAsk {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match self.scope {
            StatsScope::Run => w.u8(0),
            StatsScope::Command => w.u8(1),
            StatsScope::Member(m) => {
                w.u8(2);
                w.u32(m);
            }
        }
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<StatsAsk, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let scope = match r.u8()? {
            0 => StatsScope::Run,
            1 => StatsScope::Command,
            2 => StatsScope::Member(r.u32()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a stats scope",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(StatsAsk { id, scope })
    }
}

impl Started {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.count(self.members.len())?;
        for m in &self.members {
            w.u32(m.index);
            w.u8(m.kind as u8);
            w.text(&m.name)?;
            w.u8(u8::from(m.runs));
        }
        w.count(self.published.len())?;
        for b in &self.published {
            w.u16(b.guest_port);
            w.u8(b.proto as u8);
            w.ip(Some(b.host.ip()));
            w.u16(b.host.port());
            w.count(b.to.len())?;
            for r in &b.to {
                w.u32(r.member);
                w.u16(r.port);
            }
        }
        // Where the run's endpoint is bound and its microVM's key, appended in version 15.
        match &self.control {
            None => w.u8(0),
            Some(at) => {
                w.u8(1);
                w.ip(Some(at.address.ip()));
                w.u16(at.address.port());
                w.0.extend_from_slice(&at.vm);
            }
        }
        // The rate's period, appended in version 17 (D247).
        w.u64(u64::try_from(self.rate_period.as_nanos()).map_err(|_| WireError::TooLong)?);
        // How its microVM started, appended in version 19 (D253).
        w.u8(self.from as u8);
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<Started, WireError> {
        let mut r = Reader(payload);
        let n = r.count(4 + 1 + 4 + 1)?;
        let mut members = Vec::with_capacity(n);
        for _ in 0..n {
            members.push(MemberInfo {
                index: r.u32()?,
                kind: member_kind(r.u8()?)?,
                name: r.text()?,
                runs: r.flag()?,
            });
        }
        let n = r.count(2 + 1 + 1 + 2 + 4)?;
        let mut published = Vec::with_capacity(n);
        for _ in 0..n {
            let guest_port = r.u16()?;
            let proto = proto(r.u8()?)?;
            let ip = r.ip()?.ok_or(WireError::Tag {
                what: "a bound address",
                tag: 0,
            })?;
            let port = r.u16()?;
            let receivers = r.count(4 + 2)?;
            let mut to = Vec::with_capacity(receivers);
            for _ in 0..receivers {
                to.push(Receiving {
                    member: r.u32()?,
                    port: r.u16()?,
                });
            }
            published.push(Bound {
                guest_port,
                proto,
                host: SocketAddr::new(ip, port),
                to,
            });
        }
        let control = if r.flag()? {
            let ip = r.ip()?.ok_or(WireError::Tag {
                what: "an endpoint's address",
                tag: 0,
            })?;
            let port = r.u16()?;
            Some(ControlAt {
                address: SocketAddr::new(ip, port),
                vm: r.array::<32>()?,
            })
        } else {
            None
        };
        // A governor holds over some time: a period of none is no period.
        let rate_period = NonZeroU64::new(r.u64()?)
            .map(|ns| std::time::Duration::from_nanos(ns.get()))
            .ok_or(WireError::Tag {
                what: "a rate period",
                tag: 0,
            })?;
        let from = started_from(r.u8()?)?;
        r.end()?;
        Ok(Started {
            members,
            published,
            control,
            rate_period,
            from,
        })
    }
}

impl Request {
    /// What a warm run host is started with and holds each run it is handed to (D253's
    /// pools): this request with its members' stages, its deadline and its control ask taken
    /// out, what alone may differ between the runs of one microVM; none of a run's secrets
    /// is in it. Everything else is the microVM's: its image, size, command, ports, network,
    /// shares, nesting, credential grants and GPUs.
    pub fn exemplar(&self) -> Request {
        Request {
            members: Vec::new(),
            ready_by: None,
            control: None,
            ..self.clone()
        }
    }
}

/// [`to_client::WARMED`]'s payload: how the warm host's microVM started.
pub fn warmed(from: StartedFrom) -> [u8; 1] {
    [from as u8]
}

/// How a warm host's microVM started, as [`warmed`] wrote it.
pub fn read_warmed(payload: &[u8]) -> Result<StartedFrom, WireError> {
    match payload {
        [tag] => started_from(*tag),
        _ => Err(WireError::Tag {
            what: "a warm host's start",
            tag: payload.first().copied().unwrap_or(0),
        }),
    }
}

impl Refused {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u8(self.cause as u8);
        w.text(&self.why)?;
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<Refused, WireError> {
        let mut r = Reader(payload);
        let cause = match r.u8()? {
            0 => Cause::Request,
            1 => Cause::Image,
            2 => Cause::Widening,
            3 => Cause::Boot,
            4 => Cause::Unsupported,
            5 => Cause::Unfit,
            tag => {
                return Err(WireError::Tag {
                    what: "a refusal's cause",
                    tag,
                });
            }
        };
        let why = r.text()?;
        r.end()?;
        Ok(Refused { cause, why })
    }
}

impl Output {
    /// Its head, before its bytes: a payload is the head then the bytes, unframed, so that
    /// the run host writes a member's bytes as they came, with no copy into another buffer.
    pub fn head(member: u32, channel: Channel) -> [u8; 5] {
        let [a, b, c, d] = member.to_be_bytes();
        [a, b, c, d, channel as u8]
    }

    pub fn decode(payload: &[u8]) -> Result<Output, WireError> {
        let (head, bytes) = payload.split_first_chunk::<5>().ok_or(WireError::Short)?;
        let [a, b, c, d, ch] = *head;
        Ok(Output {
            member: u32::from_be_bytes([a, b, c, d]),
            channel: channel(ch)?,
            bytes: bytes.to_vec(),
        })
    }
}

impl Input {
    /// Its head, before its bytes, as [`Output::head`] is an output's.
    pub fn head(member: u32) -> [u8; 4] {
        member.to_be_bytes()
    }

    pub fn decode(payload: &[u8]) -> Result<Input, WireError> {
        let (head, bytes) = payload.split_first_chunk::<4>().ok_or(WireError::Short)?;
        Ok(Input {
            member: u32::from_be_bytes(*head),
            bytes: Secret(bytes.to_vec()),
        })
    }
}

impl InputEnd {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.member);
        w.stream_end(self.end);
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<InputEnd, WireError> {
        let mut r = Reader(payload);
        let member = r.u32()?;
        let end = r.stream_end("an input's end")?;
        r.end()?;
        Ok(InputEnd { member, end })
    }
}

impl InputRoom {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.member);
        w.u64(self.bytes);
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<InputRoom, WireError> {
        let mut r = Reader(payload);
        let room = InputRoom {
            member: r.u32()?,
            bytes: r.u64()?,
        };
        r.end()?;
        Ok(room)
    }
}

impl InputEnded {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.member);
        w.u64(self.written);
        w.stream_end(self.end);
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<InputEnded, WireError> {
        let mut r = Reader(payload);
        let member = r.u32()?;
        let written = r.u64()?;
        let end = r.stream_end("an input's end")?;
        r.end()?;
        Ok(InputEnded { member, written, end })
    }
}

impl OutputEnd {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.member);
        w.u8(self.channel as u8);
        match self.end {
            StreamEnd::Closed => w.u8(0),
            StreamEnd::Aborted(errno) => {
                w.u8(1);
                w.i32(errno);
            }
        }
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<OutputEnd, WireError> {
        let mut r = Reader(payload);
        let member = r.u32()?;
        let channel = channel(r.u8()?)?;
        let end = match r.u8()? {
            0 => StreamEnd::Closed,
            1 => StreamEnd::Aborted(r.i32()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a stream's end",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(OutputEnd { member, channel, end })
    }
}

impl MemberEnded {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.member);
        w.u32(self.status);
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<MemberEnded, WireError> {
        let mut r = Reader(payload);
        let e = MemberEnded {
            member: r.u32()?,
            status: r.u32()?,
        };
        r.end()?;
        Ok(e)
    }
}

impl StatsAnswer {
    /// Its id, then 0, init's lines as it said them and the run's microVM (0 for none, or 1
    /// and what it costs: its VM process's use, 0 and its CPU time, resident and own memory,
    /// or 1 and why none was read; its vCPUs' guest time, 0, what departed vCPUs ran and each
    /// vCPU's index, time and 0, or 1 and why it went unread; or 1 and why none was read), or
    /// 1 and why none came.
    pub fn encode(id: u32, result: Result<(&[u8], Option<&Microvm>), &str>) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(id);
        match result {
            Ok((lines, microvm)) => {
                w.u8(0);
                w.bytes(lines)?;
                w.microvm(microvm)?;
            }
            Err(why) => {
                w.u8(1);
                w.text(why)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<StatsAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => {
                let mut stats = Stats::parse(&String::from_utf8_lossy(r.bytes()?));
                stats.microvm = r.microvm()?;
                Ok(stats)
            }
            1 => Err(r.text()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a stats answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(StatsAnswer { id, result })
    }
}

impl RoomAsk {
    /// Its id, then 0 for no budget, or 1 and the room.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match self.room {
            None => w.u8(0),
            Some(room) => {
                w.u8(1);
                w.i64(room);
            }
        }
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<RoomAsk, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let room = match r.u8()? {
            0 => None,
            1 => Some(r.i64()?),
            tag => return Err(WireError::Tag { what: "a room", tag }),
        };
        r.end()?;
        Ok(RoomAsk { id, room })
    }
}

impl RoomAnswer {
    /// Its id, then 0 for none held, 1 and the bytes held within, or 2 and why.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match &self.result {
            Ok(None) => w.u8(0),
            Ok(Some(held)) => {
                w.u8(1);
                w.u64(*held);
            }
            Err(why) => {
                w.u8(2);
                w.text(why)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<RoomAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(r.u64()?)),
            2 => Err(r.text()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a room's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(RoomAnswer { id, result })
    }
}

impl ResizeAsk {
    /// Its id, its member, then its rows and columns.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        w.u32(self.member);
        w.u16(self.rows);
        w.u16(self.cols);
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<ResizeAsk, WireError> {
        let mut r = Reader(payload);
        let ask = ResizeAsk {
            id: r.u32()?,
            member: r.u32()?,
            rows: r.u16()?,
            cols: r.u16()?,
        };
        r.end()?;
        Ok(ask)
    }
}

impl ResizeAnswer {
    /// Its id, then 0 for sized, or 1 and why not.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match &self.result {
            Ok(()) => w.u8(0),
            Err(why) => {
                w.u8(1);
                w.text(why)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<ResizeAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => Ok(()),
            1 => Err(r.text()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a resize's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(ResizeAnswer { id, result })
    }
}

impl RateAsk {
    /// Its id, then the rate, 0 for no hold: no rate of nothing, which is a pause, not a
    /// rate.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        w.u64(self.rate.map_or(0, NonZeroU64::get));
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<RateAsk, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let rate = NonZeroU64::new(r.u64()?);
        r.end()?;
        Ok(RateAsk { id, rate })
    }
}

impl RateAnswer {
    /// Its id, then 0 and the milli-CPU the vCPUs are held to (0 for none), or 1 and why.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match &self.result {
            Ok(held) => {
                w.u8(0);
                w.u64(held.map_or(0, NonZeroU64::get));
            }
            Err(why) => {
                w.u8(1);
                w.text(why)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<RateAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => Ok(NonZeroU64::new(r.u64()?)),
            1 => Err(r.text()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a rate's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(RateAnswer { id, result })
    }
}

impl LeaseAsk {
    /// Its id, then the credential's reference.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        w.text(&self.reference)?;
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<LeaseAsk, WireError> {
        let mut r = Reader(payload);
        let ask = LeaseAsk {
            id: r.u32()?,
            reference: r.text()?,
        };
        r.end()?;
        Ok(ask)
    }
}

impl LeaseAnswer {
    /// Its id, then 0, how long the lease lives in nanoseconds and the value, last, so that
    /// the buffer holding it is made once; or its refusal's tag and fields: 1 and the
    /// broker's words, 2 and the key asked under and the run's key (0 for none, or 1 and the
    /// key), 3 and the broker's words, 4, or 5. The caller wipes what it returns.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(self.id);
        match &self.result {
            Ok(lease) => {
                w.u8(0);
                w.u64(lease.lives.get());
                w.bytes(&lease.value.0)?;
            }
            Err(LeaseRefusal::Denied(why)) => {
                w.u8(1);
                w.text(why)?;
            }
            Err(LeaseRefusal::KeyReplaced { asked, current }) => {
                w.u8(2);
                w.text(asked)?;
                match current {
                    None => w.u8(0),
                    Some(current) => {
                        w.u8(1);
                        w.text(current)?;
                    }
                }
            }
            Err(LeaseRefusal::Unavailable(why)) => {
                w.u8(3);
                w.text(why)?;
            }
            Err(LeaseRefusal::Ended) => w.u8(4),
            Err(LeaseRefusal::Lapsed) => w.u8(5),
        }
        w.done()
    }

    /// A lease that lives no time is no lease: refused, as [`Started`]'s bound address of
    /// none is.
    pub fn decode(payload: &[u8]) -> Result<LeaseAnswer, WireError> {
        let mut r = Reader(payload);
        let id = r.u32()?;
        let result = match r.u8()? {
            0 => {
                let lives = NonZeroU64::new(r.u64()?).ok_or(WireError::Tag {
                    what: "a lease's life",
                    tag: 0,
                })?;
                Ok(Lease {
                    lives,
                    value: Secret(r.bytes()?.to_vec()),
                })
            }
            1 => Err(LeaseRefusal::Denied(r.text()?)),
            2 => {
                let asked = r.text()?;
                let current = if r.flag()? { Some(r.text()?) } else { None };
                Err(LeaseRefusal::KeyReplaced { asked, current })
            }
            3 => Err(LeaseRefusal::Unavailable(r.text()?)),
            4 => Err(LeaseRefusal::Ended),
            5 => Err(LeaseRefusal::Lapsed),
            tag => {
                return Err(WireError::Tag {
                    what: "a lease's answer",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(LeaseAnswer { id, result })
    }
}

/// What a [`to_client::RELEASED`] says: every lease of the run released. Its payload is
/// none; anything else is refused.
pub fn released(payload: &[u8]) -> Result<(), WireError> {
    Reader(payload).end()
}

impl RunEnd {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        match self {
            RunEnd::Exited(status) => {
                w.u8(0);
                w.u32(*status);
            }
            RunEnd::Stopped => w.u8(1),
            RunEnd::Failed(why) => {
                w.u8(2);
                w.text(why)?;
            }
        }
        w.done()
    }

    pub fn decode(payload: &[u8]) -> Result<RunEnd, WireError> {
        let mut r = Reader(payload);
        let end = match r.u8()? {
            0 => RunEnd::Exited(r.u32()?),
            1 => RunEnd::Stopped,
            2 => RunEnd::Failed(r.text()?),
            tag => {
                return Err(WireError::Tag {
                    what: "a run's end",
                    tag,
                });
            }
        };
        r.end()?;
        Ok(end)
    }
}

/// What a [`to_client::READY`] says: its payload is [`Ready::encode`]'s nine bytes, as the
/// guest's init sent them; anything else is refused, its state's tag named where it is
/// nine bytes long (none of its versions has it, or it is on time with a lateness).
pub fn ready(payload: &[u8]) -> Result<Ready, WireError> {
    let mut r = Reader(payload);
    let state = r.u8()?;
    r.u64()?;
    r.end()?;
    Ready::decode(payload).ok_or(WireError::Tag {
        what: "a readiness",
        tag: state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plug's ask, an unplug's and their answer come back as they went, each refusal by
    /// its tag; a truncated, trailing or unknown-tagged one is refused, never a default.
    #[test]
    fn plugs_round_trip_and_bad_ones_are_refused() {
        let ask = PlugAsk {
            id: 7,
            member: 2,
            host: "/Users/u/proj".into(),
            at: "/Users/u/proj".into(),
            read_only: true,
        };
        let bytes = ask.encode().unwrap();
        assert_eq!(PlugAsk::decode(&bytes).unwrap(), ask);
        assert_eq!(PlugAsk::decode(&bytes[..bytes.len() - 1]), Err(WireError::Short));
        assert_eq!(
            PlugAsk::decode(&[bytes.as_slice(), &[0]].concat()),
            Err(WireError::Trailing)
        );
        let mut flag = bytes.clone();
        *flag.last_mut().unwrap() = 2;
        assert!(matches!(PlugAsk::decode(&flag), Err(WireError::Tag { .. })));
        let un = UnplugAsk {
            id: 8,
            at: "/Users/u/proj".into(),
        };
        assert_eq!(UnplugAsk::decode(&un.encode().unwrap()).unwrap(), un);
        for result in [
            Ok(()),
            Err((PlugRefusal::Request, "a path".to_string())),
            Err((PlugRefusal::Member, String::new())),
            Err((PlugRefusal::Outside, "the run's plug roots are /Users".into())),
            Err((PlugRefusal::Taken, String::new())),
            Err((PlugRefusal::Host, String::new())),
            Err((PlugRefusal::Device, String::new())),
            Err((PlugRefusal::Unknown, String::new())),
            Err((PlugRefusal::Busy, String::new())),
        ] {
            let answer = PlugAnswer { id: 9, result };
            assert_eq!(PlugAnswer::decode(&answer.encode().unwrap()).unwrap(), answer);
        }
        assert!(matches!(
            PlugAnswer::decode(&[0, 0, 0, 9, 1, 8, 0, 0, 0, 0]),
            Err(WireError::Tag { .. })
        ));
        assert!(matches!(
            PlugAnswer::decode(&[0, 0, 0, 9, 2]),
            Err(WireError::Tag { .. })
        ));
    }

    /// A save's ask and its answer come back as they went, each refusal by its tag (D259); a
    /// truncated, trailing or unknown-tagged one is refused, never a default.
    #[test]
    fn saves_round_trip_and_bad_ones_are_refused() {
        let ask = SaveAsk {
            id: 7,
            to: "/Users/u/Library/Application Support/shards/saves/s1".into(),
        };
        let bytes = ask.encode().unwrap();
        assert_eq!(SaveAsk::decode(&bytes).unwrap(), ask);
        assert_eq!(SaveAsk::decode(&bytes[..bytes.len() - 1]), Err(WireError::Short));
        assert_eq!(
            SaveAsk::decode(&[bytes.as_slice(), &[0]].concat()),
            Err(WireError::Trailing)
        );
        assert_eq!(
            SaveAsk::decode(&[0, 0, 0, 7, 0, 0, 0, 1, 0xff]),
            Err(WireError::Text)
        );
        for result in [
            Ok(()),
            Err((SaveRefusal::Destination, "outside the run's home".to_string())),
            Err((SaveRefusal::State, String::new())),
            Err((SaveRefusal::Unsupported, String::new())),
            Err((SaveRefusal::Failed, "its guest did not go quiet".into())),
        ] {
            let answer = SaveAnswer { id: 9, result };
            assert_eq!(SaveAnswer::decode(&answer.encode().unwrap()).unwrap(), answer);
        }
        assert!(matches!(
            SaveAnswer::decode(&[0, 0, 0, 9, 1, 4, 0, 0, 0, 0]),
            Err(WireError::Tag { .. })
        ));
        assert!(matches!(
            SaveAnswer::decode(&[0, 0, 0, 9, 2]),
            Err(WireError::Tag { .. })
        ));
        assert_eq!(SaveAnswer::decode(&[0, 0, 0, 9, 0, 0]), Err(WireError::Trailing));
    }

    /// A credential's value never prints: a [`Secret`]'s `Debug` says only its length, and
    /// a [`CredentialGrant`] carries no value to print (its reference and hosts; of its
    /// placeholder, its length alone).
    #[test]
    fn a_value_never_prints() {
        let secret = Secret(b"sk-ant-very-secret-value".to_vec());
        let shown = format!("{secret:?}");
        assert!(
            !shown.contains("sk-ant"),
            "a Secret's Debug showed its value: {shown}"
        );
        assert_eq!(shown, "Secret(24 bytes)");
        let grant = CredentialGrant {
            reference: "anthropic/claude-max".into(),
            toward: vec!["api.anthropic.com".into()],
            placeholder: "kiln-placeholder-5ec2e7".into(),
        };
        let shown = format!("{grant:?}");
        assert!(
            !shown.contains("5ec2e7"),
            "a grant's Debug showed its placeholder: {shown}"
        );
        assert!(
            shown.contains("anthropic/claude-max"),
            "the reference is a name, not a value"
        );
        assert!(shown.contains("api.anthropic.com"));
        // A staged variable holding a value is a Secret, redacted in a MemberStage's Debug.
        let staged = MemberStage {
            kind: MemberKind::Harness,
            name: "codex".into(),
            env: vec![Secret::from("OPENAI_API_KEY=sk-live-DEADBEEF")],
            files: vec![StagedFile {
                path: "auth.json".into(),
                mode: 0o600,
                bytes: Secret::from("{\"token\":\"sk-live-DEADBEEF\"}"),
            }],
            workdir: None,
            input: false,
            terminal: false,
            args: Vec::new(),
            mcp: false,
        };
        let shown = format!("{staged:?}");
        assert!(
            !shown.contains("DEADBEEF"),
            "a staged value leaked into Debug: {shown}"
        );
        // A lease's value is a Secret too, redacted in its answer's Debug (D242).
        let answer = LeaseAnswer {
            id: 1,
            result: Ok(Lease {
                value: Secret::from("sk-ant-lease-DEADBEEF"),
                lives: NonZeroU64::MIN,
            }),
        };
        let shown = format!("{answer:?}");
        assert!(
            !shown.contains("DEADBEEF"),
            "a lease's value leaked into Debug: {shown}"
        );
        assert!(shown.contains("Secret(21 bytes)"), "{shown}");
    }

    /// A lease's ask and its answer read back as written, each refusal by its tag, and the
    /// run's release as none (D242); one cut short, padded or of an unknown tag is refused,
    /// as is a lease that lives no time, never a default.
    #[test]
    fn leases_round_trip_and_bad_ones_are_refused() {
        let ask = LeaseAsk {
            id: 3,
            reference: "anthropic/claude-max".into(),
        };
        let bytes = ask.encode().unwrap();
        assert_eq!(LeaseAsk::decode(&bytes).unwrap(), ask);
        assert_eq!(LeaseAsk::decode(&bytes[..bytes.len() - 1]), Err(WireError::Short));
        assert_eq!(
            LeaseAsk::decode(&[bytes.as_slice(), &[0]].concat()),
            Err(WireError::Trailing)
        );
        let given = Lease {
            value: Secret::from("sk-ant-api03-lease"),
            lives: NonZeroU64::new(3_600_000_000_000).unwrap(),
        };
        for result in [
            Ok(given.clone()),
            Ok(Lease {
                value: Secret::default(),
                lives: NonZeroU64::MIN,
            }),
            Err(LeaseRefusal::Denied("no such secret".into())),
            Err(LeaseRefusal::KeyReplaced {
                asked: "a".repeat(64),
                current: Some("b".repeat(64)),
            }),
            Err(LeaseRefusal::KeyReplaced {
                asked: "a".repeat(64),
                current: None,
            }),
            Err(LeaseRefusal::Unavailable(String::new())),
            Err(LeaseRefusal::Ended),
            Err(LeaseRefusal::Lapsed),
        ] {
            let answer = LeaseAnswer { id: u32::MAX, result };
            let bytes = answer.encode().unwrap();
            assert_eq!(LeaseAnswer::decode(&bytes).unwrap(), answer);
            assert_eq!(
                LeaseAnswer::decode(&bytes[..bytes.len() - 1]),
                Err(WireError::Short),
                "{answer:?}"
            );
            assert_eq!(
                LeaseAnswer::decode(&[bytes.as_slice(), &[0]].concat()),
                Err(WireError::Trailing),
                "{answer:?}"
            );
        }
        assert!(matches!(
            LeaseAnswer::decode(&[0, 0, 0, 1, 6]),
            Err(WireError::Tag { tag: 6, .. })
        ));
        let mut flag = LeaseAnswer {
            id: 1,
            result: Err(LeaseRefusal::KeyReplaced {
                asked: "a".into(),
                current: None,
            }),
        }
        .encode()
        .unwrap();
        *flag.last_mut().unwrap() = 2;
        assert_eq!(
            LeaseAnswer::decode(&flag),
            Err(WireError::Tag {
                what: "a flag",
                tag: 2
            })
        );
        let mut none = LeaseAnswer {
            id: 1,
            result: Ok(given),
        }
        .encode()
        .unwrap();
        none[5..13].fill(0);
        assert_eq!(
            LeaseAnswer::decode(&none),
            Err(WireError::Tag {
                what: "a lease's life",
                tag: 0
            })
        );
        assert_eq!(released(&[]), Ok(()));
        assert_eq!(released(&[0]), Err(WireError::Trailing));
    }

    fn request() -> Request {
        Request {
            image: "registry.example/codex:1".into(),
            vcpus: 2,
            memory_mib: 2048,
            command: None,
            members: vec![MemberStage {
                kind: MemberKind::Harness,
                name: "codex".into(),
                env: vec![Secret::from("OPENAI_API_KEY=sk-test-123")],
                files: vec![StagedFile {
                    path: ".codex/auth.json".into(),
                    mode: 0o600,
                    bytes: Secret(b"{\"token\":\"t0p\"}\0".to_vec()),
                }],
                workdir: None,
                input: true,
                terminal: false,
                args: Vec::new(),
                mcp: false,
            }],
            publish: vec![
                Publish {
                    guest_port: 8080,
                    proto: Proto::Tcp,
                    host: Some("127.0.0.1".parse().unwrap()),
                    host_port: 0,
                },
                Publish {
                    guest_port: 53,
                    proto: Proto::Udp,
                    host: Some("::1".parse().unwrap()),
                    host_port: 5353,
                },
            ],
            egress: Narrowing {
                egress: Keep::Only(vec!["443/tcp".into()]),
                mcp: Keep::All,
                destinations: Keep::All,
                dns: false,
                members: None,
            },
            shares: vec![ShareAt {
                host: "/home/u/work".into(),
                at: "/work".into(),
                read_only: false,
            }],
            nesting: false,
            ready_by: None,
            credentials: Vec::new(),
            control: None,
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
            upstream_roots: Vec::new(),
        }
    }

    /// Every message reads back as written, its fields at their edges.
    #[test]
    fn messages_round_trip() {
        let r = request();
        assert_eq!(Request::decode(&r.encode().unwrap()).unwrap(), r);
        // The run's credential grants (version 8, D242): none, one with several hosts, and
        // one with none, each reading back as written, with its placeholder (version 22).
        let mut credited = request();
        credited.credentials = vec![
            CredentialGrant {
                reference: "anthropic/claude-max".into(),
                toward: vec!["api.anthropic.com".into(), "platform.claude.com".into()],
                placeholder: "kiln-placeholder-0a1b".into(),
            },
            CredentialGrant {
                reference: "openai/chatgpt".into(),
                toward: Vec::new(),
                placeholder: String::new(),
            },
        ];
        assert_eq!(Request::decode(&credited.encode().unwrap()).unwrap(), credited);
        // A placeholder is the grants' last field, before the sign-ins' count (version 24) and
        // the member's MCP bridge's flag (version 25): a request cut in it is refused.
        let bytes = credited.encode().unwrap();
        assert!(Request::decode(&bytes[..bytes.len() - 1 - 4 - 1]).is_err());
        // Each member's working directory, or none, in the members' order (version 5),
        // whether its input is the orchestrator's (version 6), and its run-time arguments, none,
        // several, or an empty one among others (version 18).
        let mut worked = request();
        worked.members[0].workdir = Some("/Users/ada/proj".into());
        worked.members[0].args = vec!["--resume".into(), "s-1".into()];
        worked.members.push(MemberStage {
            kind: MemberKind::Agent,
            name: "helper".into(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            input: false,
            terminal: false,
            args: Vec::new(),
            mcp: false,
        });
        // ... and its MCP bridge (version 25, D254).
        worked.members.push(MemberStage {
            workdir: Some("/".into()),
            input: true,
            terminal: true,
            args: vec![String::new(), "two words".into(), "--model=opüs".into()],
            mcp: true,
            ..worked.members[1].clone()
        });
        assert_eq!(Request::decode(&worked.encode().unwrap()).unwrap(), worked);
        let with_command = Request {
            command: Some(vec!["sh".into(), "-c".into(), String::new()]),
            ..request()
        };
        assert_eq!(
            Request::decode(&with_command.encode().unwrap()).unwrap(),
            with_command
        );
        // The nesting grant asked for (D234), with no deadline and at a deadline's edges;
        // and a deadline in a request that does not ask for it, which its host refuses.
        for (nesting, ready_by) in [
            (true, None),
            (true, Some(0)),
            (true, Some(u64::MAX)),
            (false, Some(1)),
        ] {
            let asked = Request {
                nesting,
                ready_by,
                ..request()
            };
            assert_eq!(Request::decode(&asked.encode().unwrap()).unwrap(), asked);
        }
        let asked = Request {
            nesting: true,
            ready_by: Some(0x0102_0304_0506_0708),
            ..request()
        }
        .encode()
        .unwrap();
        // ... the control ask's absence (version 15), the GPUs' count (version 16), the
        // member's arguments' count (version 18), the sign-ins' count (version 24) and the
        // member's MCP bridge (version 25) last.
        assert_eq!(
            asked.get(asked.len() - 33..).unwrap(),
            [
                1, 1, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0
            ],
            "appended: the grant's flag, the deadline's, the destinations kept (all), the one \
             member's working directory (none) and its input, the credential grants' count \
             (zero, version 8), the member's terminal (none, version 11), whether the members \
             are held apart (no, version 12), the control ask (none, version 15), the GPUs' \
             count (zero, version 16), the member's arguments' count (zero, version 18), then \
             the sign-ins' count (zero, version 24), then its MCP bridge (none, version 25)"
        );
        // Destinations kept by name, the host itself among them (D236), and none.
        for destinations in [
            Keep::Only(vec![
                "api.openai.com:443".into(),
                "host.docker.internal:7070".into(),
            ]),
            Keep::Only(Vec::new()),
        ] {
            let narrowed = Request {
                egress: Narrowing {
                    destinations,
                    ..request().egress
                },
                ..request()
            };
            assert_eq!(Request::decode(&narrowed.encode().unwrap()).unwrap(), narrowed);
        }
        for c in [
            Credit {
                member: 0,
                channel: Channel::Stdout,
                bytes: 0,
            },
            Credit {
                member: u32::MAX,
                channel: Channel::Stderr,
                bytes: u64::MAX,
            },
        ] {
            assert_eq!(Credit::decode(&c.encode()).unwrap(), c);
        }
        for scope in [StatsScope::Run, StatsScope::Command, StatsScope::Member(7)] {
            let ask = StatsAsk { id: 9, scope };
            assert_eq!(StatsAsk::decode(&ask.encode()).unwrap(), ask);
        }
        // A port no member claims (the run's own command's), one member's, and one two
        // members claim, one of them at a port of its own (D122).
        let started = Started {
            members: vec![MemberInfo {
                index: 1,
                kind: MemberKind::Agent,
                name: "main".into(),
                runs: false,
            }],
            published: vec![
                Bound {
                    guest_port: 8080,
                    proto: Proto::Tcp,
                    host: "[::1]:40000".parse().unwrap(),
                    to: Vec::new(),
                },
                Bound {
                    guest_port: 53,
                    proto: Proto::Udp,
                    host: "127.0.0.1:5353".parse().unwrap(),
                    to: vec![Receiving { member: 1, port: 53 }],
                },
                Bound {
                    guest_port: 8081,
                    proto: Proto::Tcp,
                    host: "0.0.0.0:8081".parse().unwrap(),
                    to: vec![
                        Receiving { member: 0, port: 80 },
                        Receiving {
                            member: u32::MAX,
                            port: u16::MAX,
                        },
                    ],
                },
            ],
            control: None,
            rate_period: std::time::Duration::from_millis(100),
            from: StartedFrom::Saved,
        };
        assert_eq!(Started::decode(&started.encode().unwrap()).unwrap(), started);
        let bytes = started.encode().unwrap();
        for cut in 0..bytes.len() {
            assert!(
                Started::decode(bytes.get(..cut).unwrap()).is_err(),
                "cut at {cut}"
            );
        }
        let refused = Refused {
            cause: Cause::Widening,
            why: "egress 22/tcp: the image grants none".into(),
        };
        assert_eq!(Refused::decode(&refused.encode().unwrap()).unwrap(), refused);
        let mut out = Output::head(3, Channel::Stderr).to_vec();
        out.extend_from_slice(b"\xff\x00no newline");
        assert_eq!(
            Output::decode(&out).unwrap(),
            Output {
                member: 3,
                channel: Channel::Stderr,
                bytes: b"\xff\x00no newline".to_vec()
            }
        );
        for end in [StreamEnd::Closed, StreamEnd::Aborted(libc_eio())] {
            let e = OutputEnd {
                member: 1,
                channel: Channel::Stdout,
                end,
            };
            assert_eq!(OutputEnd::decode(&e.encode()).unwrap(), e);
        }
        let ended = MemberEnded {
            member: 2,
            status: 137,
        };
        assert_eq!(MemberEnded::decode(&ended.encode()).unwrap(), ended);
        for end in [RunEnd::Exited(0), RunEnd::Stopped, RunEnd::Failed("x".into())] {
            assert_eq!(RunEnd::decode(&end.encode().unwrap()).unwrap(), end);
        }
        let answer =
            StatsAnswer::decode(&StatsAnswer::encode(4, Ok((b"cpu 10\npids 3\n", None))).unwrap()).unwrap();
        assert_eq!(answer.id, 4);
        let stats = answer.result.unwrap();
        assert_eq!(
            (stats.cpu_ns, stats.pids, stats.memory_bytes, stats.microvm),
            (Some(10), Some(3), None, None)
        );
        let refusal = StatsAnswer::decode(&StatsAnswer::encode(5, Err("no member 9 runs")).unwrap()).unwrap();
        assert_eq!(refusal.result, Err("no member 9 runs".into()));
    }

    fn libc_eio() -> i32 {
        5
    }

    /// A member's input both ways round-trips: its bytes as they are, its ends typed, a flag
    /// or tag no version has refused.
    #[test]
    fn a_members_input_round_trips_and_its_malformed_frames_are_refused() {
        let mut bytes = Input::head(7).to_vec();
        bytes.extend_from_slice(b"\0\xff\x80{\"id\":1}\nno-newline");
        assert_eq!(
            Input::decode(&bytes).unwrap(),
            Input {
                member: 7,
                bytes: Secret(b"\0\xff\x80{\"id\":1}\nno-newline".to_vec())
            }
        );
        assert_eq!(Input::decode(&[0, 0, 1]), Err(WireError::Short));
        for end in [StreamEnd::Closed, StreamEnd::Aborted(libc_eio())] {
            let e = InputEnd { member: 2, end };
            assert_eq!(InputEnd::decode(&e.encode()).unwrap(), e);
            let ended = InputEnded {
                member: 2,
                written: u64::MAX,
                end,
            };
            assert_eq!(InputEnded::decode(&ended.encode()).unwrap(), ended);
        }
        let room = InputRoom {
            member: 4,
            bytes: 65536,
        };
        assert_eq!(InputRoom::decode(&room.encode()).unwrap(), room);
        let mut long = room.encode();
        long.push(0);
        assert_eq!(InputRoom::decode(&long), Err(WireError::Trailing));
        assert!(matches!(
            InputEnd::decode(&[0, 0, 0, 1, 2]),
            Err(WireError::Tag { tag: 2, .. })
        ));
        // A member's input flag, before the credential grants' count (version 8), its
        // terminal's flag (version 11) and the members' reach (version 12), that is no flag.
        // One member, so its input is the twentieth byte from the end (the count's four, its
        // terminal's one, the reach's flag, the control ask's, the GPUs' count, its arguments'
        // count and the sign-ins' count follow it).
        let mut bad = request().encode().unwrap();
        let at = bad.len() - 21;
        assert_eq!(
            bad[at], 1,
            "the member's input flag precedes the credentials' count"
        );
        bad[at] = 2;
        assert!(matches!(
            Request::decode(&bad),
            Err(WireError::Tag {
                what: "a flag",
                tag: 2
            })
        ));
    }

    /// A request cut anywhere short, or with a byte past its end, is refused by type; no
    /// prefix is ever read as a request.
    #[test]
    fn a_cut_or_padded_request_is_refused() {
        let bytes = request().encode().unwrap();
        for cut in 0..bytes.len() {
            assert!(
                Request::decode(bytes.get(..cut).unwrap()).is_err(),
                "cut at {cut}"
            );
        }
        let mut padded = bytes.clone();
        padded.push(0);
        assert_eq!(Request::decode(&padded), Err(WireError::Trailing));
    }

    /// Each member's reach of the names kept (version 12, D246). Do: hold the members apart
    /// with no member named, with one reaching a provider and the host itself, and with two,
    /// one reaching nothing; and hold none. Expect: each reads back as written, its reach
    /// last, and one cut anywhere short refused. Do: write the reach's flag as no flag, a
    /// member of no kind, and a count of four billion members in a few bytes. Expect: each
    /// refused by type, never read as another request.
    #[test]
    fn each_members_reach_rides_the_request_last() {
        let reach = |kind, name: &str, destinations: &[&str]| MemberReach {
            kind,
            name: name.into(),
            destinations: destinations.iter().map(|d| (*d).to_string()).collect(),
        };
        let held = |members: Option<Vec<MemberReach>>| Request {
            egress: Narrowing {
                destinations: Keep::Only(vec![
                    "api.openai.com:443".into(),
                    "host.docker.internal:7070".into(),
                ]),
                members,
                ..request().egress
            },
            ..request()
        };
        for members in [
            None,
            Some(Vec::new()),
            Some(vec![reach(
                MemberKind::Harness,
                "codex",
                &["api.openai.com:443", "host.docker.internal:7070"],
            )]),
            Some(vec![
                reach(MemberKind::Harness, "codex", &["api.openai.com:443"]),
                reach(MemberKind::Agent, "helper", &[]),
            ]),
        ] {
            let asked = held(members);
            let bytes = asked.encode().unwrap();
            assert_eq!(bytes[0], VERSION);
            assert_eq!(Request::decode(&bytes).unwrap(), asked);
            for cut in 0..bytes.len() {
                assert!(
                    Request::decode(bytes.get(..cut).unwrap()).is_err(),
                    "cut at {cut}"
                );
            }
        }
        // The reach's flag, before the control ask's, the GPUs' count, the member's arguments'
        // count and the sign-ins' count where none is held apart.
        let mut flag = held(None).encode().unwrap();
        let at = flag.len() - 15;
        flag[at] = 2;
        assert_eq!(
            Request::decode(&flag),
            Err(WireError::Tag {
                what: "a flag",
                tag: 2
            })
        );
        // One member reaching nothing ends in its kind, its name and its names' count.
        let mut kindless = held(Some(vec![reach(MemberKind::Agent, "helper", &[])]))
            .encode()
            .unwrap();
        // ... then the control ask's absence (version 15), the GPUs' count (version 16), the
        // one member's arguments' count (version 18) and the sign-ins' count (version 24).
        let at = kindless.len() - (1 + 4 + "helper".len() + 4 + 1 + 4 + 4 + 4 + 1);
        assert_eq!(kindless[at], MemberKind::Agent as u8);
        kindless[at] = 2;
        assert_eq!(
            Request::decode(&kindless),
            Err(WireError::Tag {
                what: "a member's kind",
                tag: 2
            })
        );
        // Held apart, with four billion members in a few bytes, in place of the reach's flag
        // and what follows it.
        let mut hostile = held(None).encode().unwrap();
        hostile.truncate(hostile.len() - 15);
        hostile.push(1);
        hostile.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(Request::decode(&hostile), Err(WireError::Short));
    }

    /// Each member's run-time arguments (version 18, D256). Do: give two members none, an
    /// empty one, and several (a space, `=`, past ASCII). Expect: each reads back exactly, each
    /// member's own in its order, last in the request, and one cut anywhere short refused. Do:
    /// write an argument that is no UTF-8, and a count of four billion arguments in a few
    /// bytes. Expect: each refused by type. Do: arguments past the channel's bound, one alone
    /// or two halves. Expect: the request refused `TooLong` as it is written, never cut to fit.
    #[test]
    fn each_members_arguments_ride_the_request_last() {
        const {
            assert!(
                VERSION >= 18,
                "the channel's version 18 carries each member's arguments"
            )
        };
        let with = |first: &[&str], second: &[&str]| {
            let mut r = request();
            let mut helper = r.members[0].clone();
            helper.kind = MemberKind::Agent;
            helper.name = "helper".into();
            helper.args = second.iter().map(|a| (*a).to_string()).collect();
            r.members[0].args = first.iter().map(|a| (*a).to_string()).collect();
            r.members.push(helper);
            r
        };
        let cases: [(&[&str], &[&str]); 3] = [
            (&[], &[]),
            (&[""], &[]),
            (
                &["--resume", "s-1", "--model", "claude-opus-4"],
                &["--permission-mode=plan", "two words", "ü"],
            ),
        ];
        for (first, second) in cases {
            let asked = with(first, second);
            let bytes = asked.encode().unwrap();
            assert_eq!(Request::decode(&bytes).unwrap(), asked);
            for cut in 0..bytes.len() {
                assert!(
                    Request::decode(bytes.get(..cut).unwrap()).is_err(),
                    "cut at {cut}"
                );
            }
        }
        // The second member's one argument, "ü", last before the sign-ins' count (version 24):
        // its bytes made no UTF-8.
        let mut bad = with(&["--resume"], &["ü"]).encode().unwrap();
        let at = bad.len() - 2 - 4 - "ü".len();
        bad[at] = 0xff;
        assert_eq!(Request::decode(&bad), Err(WireError::Text));
        // The second member's count, before the sign-ins', said as four billion in no bytes.
        let mut hostile = with(&[], &[]).encode().unwrap();
        hostile.truncate(hostile.len() - 2 - 4);
        let at = hostile.len() - 4;
        hostile[at..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(Request::decode(&hostile), Err(WireError::Short));
        // Past the channel's bound, one argument alone or two halves: refused, never cut.
        let most = run::MAX_PAYLOAD as usize;
        let mut past = request();
        past.members[0].args = vec!["x".repeat(most)];
        assert_eq!(past.encode(), Err(WireError::TooLong));
        past.members[0].args = vec!["x".repeat(most / 2), "x".repeat(most / 2)];
        assert_eq!(past.encode(), Err(WireError::TooLong));
    }

    /// Unknown versions and tags are refused, never defaulted.
    #[test]
    fn unknown_versions_and_tags_are_refused() {
        let mut bytes = request().encode().unwrap();
        bytes[0] = VERSION + 1;
        assert_eq!(Request::decode(&bytes), Err(WireError::Version(VERSION + 1)));
        // A request of version 1, without the nesting grant's fields and the destinations',
        // of version 4, without its members' working directories, or of version 5, without
        // their inputs, is no request of this one's: refused by its version, never read with
        // them defaulted. The request's one member makes each of those one byte, its
        // arguments' count (version 18) four bytes, which every version before 18 lacks, and the
        // sign-ins' count (version 24) four, which every version before 24 lacks.
        let mut v1 = request().encode().unwrap();
        v1.truncate(v1.len() - 25);
        v1[0] = 1;
        assert_eq!(Request::decode(&v1), Err(WireError::Version(1)));
        let mut v4 = request().encode().unwrap();
        v4.truncate(v4.len() - 22);
        v4[0] = 4;
        assert_eq!(Request::decode(&v4), Err(WireError::Version(4)));
        let mut v5 = request().encode().unwrap();
        v5.truncate(v5.len() - 21);
        v5[0] = 5;
        assert_eq!(Request::decode(&v5), Err(WireError::Version(5)));
        // One of version 6 carries every field version 7 does, but its host ends the run at
        // a room: refused by its version, before any room is told (D241).
        let mut v6 = request().encode().unwrap();
        v6.truncate(v6.len() - 20);
        v6[0] = 6;
        assert_eq!(Request::decode(&v6), Err(WireError::Version(6)));
        // One of version 7, without the credential grants' count: refused by its version
        // (D242), so an older reader never reads a grant with its field defaulted.
        let mut v7 = request().encode().unwrap();
        v7.truncate(v7.len() - 20);
        v7[0] = 7;
        assert_eq!(Request::decode(&v7), Err(WireError::Version(7)));
        // One of version 8, without its members' terminals: refused by its version (D244), so
        // an older reader never starts a member with pipes where its owner asked a terminal.
        let mut v8 = request().encode().unwrap();
        v8.truncate(v8.len() - 16);
        v8[0] = 8;
        assert_eq!(Request::decode(&v8), Err(WireError::Version(8)));
        // One of version 11, without the members' reach: refused by its version (D246), so a
        // run host never takes a request that holds its members apart as one that holds none.
        let mut v11 = request().encode().unwrap();
        v11.truncate(v11.len() - 15);
        v11[0] = 11;
        assert_eq!(Request::decode(&v11), Err(WireError::Version(11)));
        // One of version 12 carries every field version 13 does, but its host ends the run at
        // a rate: refused by its version, before any rate is told (D247).
        let mut v12 = request().encode().unwrap();
        v12.truncate(v12.len() - 14);
        v12[0] = 12;
        assert_eq!(Request::decode(&v12), Err(WireError::Version(12)));
        // One of version 14, without the control ask: refused by its version (D252), so a run
        // host never takes a run that asked an endpoint as one that asked none.
        let mut v14 = request().encode().unwrap();
        v14.truncate(v14.len() - 14);
        v14[0] = 14;
        assert_eq!(Request::decode(&v14), Err(WireError::Version(14)));
        // One of version 15, without the GPUs it names: refused by its version (D232), so a run
        // host never starts a run without the GPUs its orchestrator asked.
        let mut v15 = request().encode().unwrap();
        v15.truncate(v15.len() - 13);
        v15[0] = 15;
        assert_eq!(Request::decode(&v15), Err(WireError::Version(15)));
        // One of version 16 asks what version 17 does, but its host tells no rate's period as
        // the run starts: refused by its version, so no orchestrator waits on one (D247).
        let mut v16 = request().encode().unwrap();
        v16.truncate(v16.len() - 9);
        v16[0] = 16;
        assert_eq!(Request::decode(&v16), Err(WireError::Version(16)));
        // One of version 17, without its members' arguments: refused by its version (D256), so
        // a run host never starts a member without the flags its orchestrator asked.
        let mut v17 = request().encode().unwrap();
        v17.truncate(v17.len() - 9);
        v17[0] = 17;
        assert_eq!(Request::decode(&v17), Err(WireError::Version(17)));
        // One of version 23, without the sign-ins' count: refused by its version (D242 step 8),
        // so a run host never takes a run that signs in as one that signs nothing in.
        let mut v23 = request().encode().unwrap();
        v23.truncate(v23.len() - 1 - 4);
        v23[0] = 23;
        assert_eq!(Request::decode(&v23), Err(WireError::Version(23)));
        // One of version 24, without its members' MCP bridges: refused by its version (D254),
        // so a run host never starts a harness without the bridge its MCP config names.
        let mut v24 = request().encode().unwrap();
        v24.truncate(v24.len() - 1);
        v24[0] = 24;
        assert_eq!(Request::decode(&v24), Err(WireError::Version(24)));
        // One of version 13 reads a vCPU's unread guest time as free words
        // (guest-cpu-time-v1), and one of 19 carries every field version 20 does, but its
        // client ends the run at a lease its host asks: each refused by its version, before
        // any lease is (D242).
        for old in [13, 19] {
            let mut bytes = request().encode().unwrap();
            bytes[0] = old;
            assert_eq!(Request::decode(&bytes), Err(WireError::Version(old)));
        }
        // The nesting flag, before the deadline's tag, the destinations', the one member's
        // working directory's and input's, the credential grants' count (version 8), the
        // member's terminal's (version 11), the members' reach (version 12), the control ask
        // (version 15), the GPUs' count (version 16), its arguments' count (version 18) and the
        // sign-ins' count (version 24): no flag.
        let mut flag = request().encode().unwrap();
        let at = flag.len() - 25;
        flag[at] = 2;
        assert_eq!(
            Request::decode(&flag),
            Err(WireError::Tag {
                what: "a flag",
                tag: 2
            })
        );
        // The destinations' list, before the member's working directory and input, the
        // credential grants' count, the member's terminal, the members' reach, the control ask,
        // the GPUs' count, the member's arguments' count and the sign-ins' count: no list of
        // version 3's and on.
        let mut list = request().encode().unwrap();
        let at = list.len() - 23;
        list[at] = 2;
        assert_eq!(
            Request::decode(&list),
            Err(WireError::Tag {
                what: "a grant list",
                tag: 2
            })
        );
        assert!(matches!(
            Credit::decode(&[0, 0, 0, 1, 3, 0, 0, 0, 0, 0, 0, 0, 1]),
            Err(WireError::Tag {
                what: "a channel",
                tag: 3
            })
        ));
        assert!(matches!(
            Refused::decode(&[9, 0, 0, 0, 0]),
            Err(WireError::Tag { tag: 9, .. })
        ));
        assert!(matches!(
            StatsAsk::decode(&[0, 0, 0, 1, 3]),
            Err(WireError::Tag { .. })
        ));
    }

    /// A hostile count allocates nothing it cannot fill: a request claiming four billion
    /// members in a few bytes is short, at once.
    #[test]
    fn a_hostile_count_is_short() {
        let mut w = Writer(Vec::new());
        w.u8(VERSION);
        w.text("i").unwrap();
        w.u32(1);
        w.u64(64);
        w.u8(0);
        w.u32(u32::MAX);
        assert_eq!(Request::decode(&w.0), Err(WireError::Short));
        // A port said to reach four billion members, in a few bytes.
        let mut w = Writer(Vec::new());
        w.u32(0);
        w.u32(1);
        w.u16(80);
        w.u8(Proto::Tcp as u8);
        w.ip(Some("127.0.0.1".parse().unwrap()));
        w.u16(8080);
        w.u32(u32::MAX);
        assert_eq!(Started::decode(&w.0), Err(WireError::Short));
    }

    /// A staged credential never reaches Debug.
    #[test]
    fn secrets_are_not_printed() {
        let shown = format!("{:?}", request());
        assert!(!shown.contains("sk-test-123"), "{shown}");
        assert!(!shown.contains("t0p"), "{shown}");
        assert!(shown.contains("Secret(26 bytes)"), "{shown}");
    }

    /// READY reads back each readiness the guest's init says (D234), how far past its
    /// deadline whole; a payload that is no readiness, short, long, of no state or on time
    /// with a lateness, is refused by type, never read as one.
    #[test]
    fn readiness_reads_as_its_guest_said_it() {
        for said in [
            Ready::OnTime,
            Ready::Late(1),
            Ready::Late(u64::MAX),
            Ready::Passed(0),
            Ready::Passed(7),
        ] {
            assert_eq!(ready(&said.encode()), Ok(said));
        }
        assert_eq!(ready(&[]), Err(WireError::Short));
        assert_eq!(ready(&[2, 0, 0, 0, 0, 0, 0, 0]), Err(WireError::Short));
        assert_eq!(ready(&[2; 10]), Err(WireError::Trailing));
        for (payload, tag) in [
            ([0; 9], 0),
            ([4, 0, 0, 0, 0, 0, 0, 0, 0], 4),
            ([1, 0, 0, 0, 0, 0, 0, 0, 1], 1),
        ] {
            assert_eq!(
                ready(&payload),
                Err(WireError::Tag {
                    what: "a readiness",
                    tag
                }),
                "{payload:?}"
            );
        }
    }

    /// A room and its answer read back as they were said (D241): no budget, none left, one
    /// past what a run holds, one short of it, each bound of an i64; none held, the bytes
    /// held within and why not. Cut short, padded or of a tag no version has, refused.
    #[test]
    fn rooms_and_their_answers_read_back_as_said() {
        for room in [
            None,
            Some(0),
            Some(64 << 20),
            Some(-(1 << 20)),
            Some(i64::MIN),
            Some(i64::MAX),
        ] {
            let ask = RoomAsk { id: 7, room };
            assert_eq!(RoomAsk::decode(&ask.encode()), Ok(ask));
        }
        for result in [
            Ok(None),
            Ok(Some(0)),
            Ok(Some(u64::MAX)),
            Err("not held: no memory.high".to_string()),
        ] {
            let answer = RoomAnswer { id: u32::MAX, result };
            assert_eq!(RoomAnswer::decode(&answer.encode().unwrap()), Ok(answer));
        }
        let ask = RoomAsk {
            id: 1,
            room: Some(-5),
        }
        .encode();
        for cut in 0..ask.len() {
            assert_eq!(
                RoomAsk::decode(&ask[..cut]),
                Err(WireError::Short),
                "cut at {cut}"
            );
        }
        let mut padded = ask.clone();
        padded.push(0);
        assert_eq!(RoomAsk::decode(&padded), Err(WireError::Trailing));
        assert_eq!(
            RoomAsk::decode(&[0, 0, 0, 1, 2]),
            Err(WireError::Tag {
                what: "a room",
                tag: 2
            })
        );
        assert_eq!(
            RoomAnswer::decode(&[0, 0, 0, 1, 3]),
            Err(WireError::Tag {
                what: "a room's answer",
                tag: 3
            })
        );
        assert_eq!(RoomAnswer::decode(&[0, 0, 0, 1, 1, 0]), Err(WireError::Short));
        assert_eq!(RoomAnswer::decode(&[0, 0, 0, 1, 0, 0]), Err(WireError::Trailing));
        let long = RoomAnswer {
            id: 1,
            result: Err("x".repeat(run::MAX_PAYLOAD as usize)),
        };
        assert_eq!(long.encode(), Err(WireError::TooLong));
    }

    /// A member's terminal's size, and its answer, read back as said at their bounds; one
    /// cut short, padded, of an unknown tag or too long is refused by type (D244). A
    /// request's terminal flag (version 11), before its members' reach (version 12), that is
    /// no flag is refused.
    #[test]
    fn resizes_and_their_answers_read_back_as_said() {
        for (member, rows, cols) in [(0, 0, 0), (3, 40, 120), (u32::MAX, u16::MAX, u16::MAX)] {
            let ask = ResizeAsk {
                id: 9,
                member,
                rows,
                cols,
            };
            assert_eq!(ResizeAsk::decode(&ask.encode()), Ok(ask));
        }
        for result in [
            Ok(()),
            Err("harness codex: it has no terminal of its own".to_string()),
        ] {
            let answer = ResizeAnswer { id: u32::MAX, result };
            assert_eq!(ResizeAnswer::decode(&answer.encode().unwrap()), Ok(answer));
        }
        let ask = ResizeAsk {
            id: 1,
            member: 2,
            rows: 24,
            cols: 80,
        }
        .encode();
        for cut in 0..ask.len() {
            assert_eq!(
                ResizeAsk::decode(&ask[..cut]),
                Err(WireError::Short),
                "cut at {cut}"
            );
        }
        let mut padded = ask.clone();
        padded.push(0);
        assert_eq!(ResizeAsk::decode(&padded), Err(WireError::Trailing));
        assert_eq!(
            ResizeAnswer::decode(&[0, 0, 0, 1, 2]),
            Err(WireError::Tag {
                what: "a resize's answer",
                tag: 2
            })
        );
        assert_eq!(ResizeAnswer::decode(&[0, 0, 0, 1, 1, 0]), Err(WireError::Short));
        assert_eq!(
            ResizeAnswer::decode(&[0, 0, 0, 1, 0, 0]),
            Err(WireError::Trailing)
        );
        let long = ResizeAnswer {
            id: 1,
            result: Err("x".repeat(run::MAX_PAYLOAD as usize)),
        };
        assert_eq!(long.encode(), Err(WireError::TooLong));
        let mut bad = request().encode().unwrap();
        let at = bad.len() - 16;
        assert_eq!(
            bad[at], 0,
            "the member's terminal flag precedes the members' reach, the control ask, the GPUs, \
             its arguments and the sign-ins"
        );
        bad[at] = 2;
        assert_eq!(
            Request::decode(&bad),
            Err(WireError::Tag {
                what: "a flag",
                tag: 2
            })
        );
    }

    /// A rate and its answer read back as they were said (D247): no hold, the least rate,
    /// a CPU and a half, the most a u64 holds; held to none, to a rate, and why not. Cut
    /// short, padded or of a tag no version has, refused.
    #[test]
    fn rates_and_their_answers_read_back_as_said() {
        for rate in [
            None,
            NonZeroU64::new(1),
            NonZeroU64::new(1500),
            NonZeroU64::new(u64::MAX),
        ] {
            let ask = RateAsk { id: 7, rate };
            assert_eq!(RateAsk::decode(&ask.encode()), Ok(ask));
        }
        for result in [
            Ok(None),
            Ok(NonZeroU64::new(1)),
            Ok(NonZeroU64::new(u64::MAX)),
            Err("the microVM has not started".to_string()),
        ] {
            let answer = RateAnswer { id: u32::MAX, result };
            assert_eq!(RateAnswer::decode(&answer.encode().unwrap()), Ok(answer));
        }
        // A rate of nothing reads as no hold: the wire has no rate of no CPU to say.
        assert_eq!(
            RateAsk::decode(&[0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0]),
            Ok(RateAsk { id: 3, rate: None })
        );
        let ask = RateAsk {
            id: 1,
            rate: NonZeroU64::new(250),
        }
        .encode();
        for cut in 0..ask.len() {
            assert_eq!(
                RateAsk::decode(&ask[..cut]),
                Err(WireError::Short),
                "cut at {cut}"
            );
        }
        let mut padded = ask.clone();
        padded.push(0);
        assert_eq!(RateAsk::decode(&padded), Err(WireError::Trailing));
        let answer = RateAnswer {
            id: 1,
            result: Ok(NonZeroU64::new(250)),
        }
        .encode()
        .unwrap();
        for cut in 0..answer.len() {
            assert_eq!(
                RateAnswer::decode(&answer[..cut]),
                Err(WireError::Short),
                "cut at {cut}"
            );
        }
        let mut padded = answer.clone();
        padded.push(0);
        assert_eq!(RateAnswer::decode(&padded), Err(WireError::Trailing));
        assert_eq!(
            RateAnswer::decode(&[0, 0, 0, 1, 2]),
            Err(WireError::Tag {
                what: "a rate's answer",
                tag: 2
            })
        );
        let long = RateAnswer {
            id: 1,
            result: Err("x".repeat(run::MAX_PAYLOAD as usize)),
        };
        assert_eq!(long.encode(), Err(WireError::TooLong));
    }

    /// The period a run's rate is held over reads back as its host said it, from a
    /// nanosecond to the most a u64 counts (D247), its eight bytes last; a period of none, or
    /// one past what a u64 counts in nanoseconds, is no period, refused each way.
    #[test]
    fn a_runs_rate_period_reads_back_as_its_host_said_it() {
        use std::time::Duration;
        let started = |rate_period| Started {
            members: Vec::new(),
            published: Vec::new(),
            control: None,
            rate_period,
            from: StartedFrom::Booted,
        };
        for period in [
            Duration::from_nanos(1),
            Duration::from_millis(100),
            Duration::from_nanos(u64::MAX),
        ] {
            let said = started(period);
            let bytes = said.encode().unwrap();
            let last = bytes.get(bytes.len() - 9..bytes.len() - 1).unwrap();
            assert_eq!(last, u64::try_from(period.as_nanos()).unwrap().to_be_bytes());
            assert_eq!(Started::decode(&bytes).unwrap(), said);
        }
        let mut none = started(Duration::from_millis(100)).encode().unwrap();
        let at = none.len() - 9;
        none.get_mut(at..at + 8).unwrap().fill(0);
        assert_eq!(
            Started::decode(&none),
            Err(WireError::Tag {
                what: "a rate period",
                tag: 0
            })
        );
        assert_eq!(
            started(Duration::from_nanos(u64::MAX) + Duration::from_nanos(1)).encode(),
            Err(WireError::TooLong)
        );
        assert_eq!(started(Duration::MAX).encode(), Err(WireError::TooLong));
    }

    /// How a run's microVM started (D253) reads back as its host said it, its byte last; a
    /// byte that names no way is refused, as a host of a later version's would be read.
    #[test]
    fn how_a_run_started_reads_back_as_its_host_said_it() {
        const {
            assert!(
                VERSION >= 19,
                "the channel's version 19 carries how a run started"
            )
        };
        for from in [
            StartedFrom::Booted,
            StartedFrom::Saved,
            StartedFrom::Restored,
            StartedFrom::Replaced,
            StartedFrom::Warm,
        ] {
            let said = Started {
                members: Vec::new(),
                published: Vec::new(),
                control: None,
                rate_period: std::time::Duration::from_millis(100),
                from,
            };
            let mut bytes = said.encode().unwrap();
            assert_eq!(bytes.last(), Some(&(from as u8)));
            assert_eq!(Started::decode(&bytes).unwrap(), said);
            if let Some(last) = bytes.last_mut() {
                *last = 5;
            }
            assert_eq!(
                Started::decode(&bytes),
                Err(WireError::Tag {
                    what: "how a run started",
                    tag: 5
                })
            );
        }
    }

    /// D253's pools: a warm run host's exemplar is its request with its members' stages, its
    /// deadline and its control ask taken out, all else kept, so that two runs alike in all
    /// but those share it; its start's one byte reads back, and a refusal of a run of another
    /// microVM's is `Unfit`.
    #[test]
    fn a_warm_hosts_exemplar_keeps_all_but_a_runs_own() {
        const { assert!(VERSION >= 21, "the channel's version 21 carries warm run hosts") };
        let full = Request {
            image: "agents/codex:1".into(),
            vcpus: 2,
            memory_mib: 1024,
            command: Some(vec!["node".into()]),
            members: vec![MemberStage {
                kind: MemberKind::Harness,
                name: "codex".into(),
                env: vec![Secret(b"KEY=secret".to_vec())],
                files: Vec::new(),
                workdir: None,
                input: false,
                terminal: false,
                args: Vec::new(),
                mcp: false,
            }],
            publish: Vec::new(),
            egress: Narrowing::default(),
            shares: Vec::new(),
            nesting: true,
            ready_by: Some(7),
            credentials: Vec::new(),
            control: Some(ControlAsk {
                controller: [1; 32],
                sockets: None,
            }),
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
            upstream_roots: Vec::new(),
        };
        let exemplar = full.exemplar();
        assert!(exemplar.members.is_empty() && exemplar.ready_by.is_none() && exemplar.control.is_none());
        assert_eq!(
            Request {
                members: full.members.clone(),
                ready_by: full.ready_by,
                control: full.control.clone(),
                ..exemplar.clone()
            },
            full
        );
        let other = Request {
            members: Vec::new(),
            ready_by: Some(9),
            control: None,
            ..full.clone()
        };
        assert_eq!(other.exemplar(), exemplar);
        assert_ne!(
            Request {
                vcpus: 4,
                ..full.clone()
            }
            .exemplar(),
            exemplar
        );
        assert_eq!(Request::decode(&exemplar.encode().unwrap()).unwrap(), exemplar);
        for from in [StartedFrom::Saved, StartedFrom::Restored, StartedFrom::Warm] {
            assert_eq!(read_warmed(&warmed(from)), Ok(from));
        }
        assert!(read_warmed(&[]).is_err() && read_warmed(&[1, 2]).is_err() && read_warmed(&[9]).is_err());
        let unfit = Refused {
            cause: Cause::Unfit,
            why: "another microVM's".into(),
        };
        assert_eq!(Refused::decode(&unfit.encode().unwrap()).unwrap(), unfit);
    }

    /// The run's stats carry what its microVM costs its host and what its vCPUs ran of its
    /// guest (D241) as its run host read them: either part unread and why, a vCPU unread and
    /// why, the largest numbers a u64 holds. vCPUs out of index order, a part's unknown tag, a count
    /// past what the bytes left could hold, cut short or padded: refused, never read as a
    /// cost.
    #[test]
    fn a_runs_microvm_reads_back_as_its_host_said_it() {
        let times = GuestTimes {
            vcpus: vec![
                VcpuTime {
                    index: 0,
                    guest_ns: 4_000,
                    unread: None,
                },
                VcpuTime {
                    index: 2,
                    guest_ns: u64::MAX,
                    unread: Some(shards_abi::guest_time::Unread::HvfExecTime(-85377023)),
                },
            ],
            departed_ns: 1_500,
        };
        assert_eq!(times.total_ns(), u64::MAX);
        let host = HostUse {
            cpu_ns: 9_000,
            resident_bytes: 64 << 20,
            footprint_bytes: u64::MAX,
        };
        let lines = b"cpu 10\nmemory 7\n";
        let gone = || Err::<HostUse, String>("its VM process: no such process".into());
        let stopped = || Err::<GuestTimes, String>("its microVM stopped".into());
        for microvm in [
            Microvm {
                host: gone(),
                guest: stopped(),
            },
            Microvm {
                host: Ok(host),
                guest: stopped(),
            },
            Microvm {
                host: gone(),
                guest: Ok(times.clone()),
            },
            Microvm {
                host: Ok(host),
                guest: Ok(times.clone()),
            },
        ] {
            let bytes = StatsAnswer::encode(3, Ok((lines, Some(&microvm)))).unwrap();
            let stats = StatsAnswer::decode(&bytes).unwrap().result.unwrap();
            assert_eq!((stats.cpu_ns, stats.memory_bytes), (Some(10), Some(7)));
            assert_eq!(stats.microvm, Some(microvm));
            for cut in 0..bytes.len() {
                assert!(StatsAnswer::decode(&bytes[..cut]).is_err(), "cut at {cut}");
            }
            let mut padded = bytes.clone();
            padded.push(0);
            assert_eq!(StatsAnswer::decode(&padded), Err(WireError::Trailing));
        }
        let both = Microvm {
            host: Ok(host),
            guest: Ok(times),
        };
        let bytes = StatsAnswer::encode(3, Ok((lines, Some(&both)))).unwrap();
        // The second vCPU's index, after the stats' id, tag, lines, the microVM's tag, its
        // host's tag and three numbers, its guest's tag, departed time and count, and the
        // first vCPU's index, time and flag: made the first's, out of order.
        let second = 4 + 1 + 4 + lines.len() + 1 + 1 + 24 + 1 + 8 + 4 + 13;
        let mut out_of_order = bytes.clone();
        out_of_order[second..second + 4].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            StatsAnswer::decode(&out_of_order),
            Err(WireError::Order {
                what: "a microVM's vCPUs"
            })
        );
        // A vCPU's unread cause only in its text form: a code signed `+`, which no side writes,
        // is refused, not taken as words.
        let token = b"hvf-exec-time -85377023";
        let at = bytes.windows(token.len()).position(|w| w == token).unwrap();
        let mut free = bytes.clone();
        free[at + 14] = b'+';
        assert_eq!(StatsAnswer::decode(&free), Err(WireError::Unread));
        let microvm_tag = 4 + 1 + 4 + lines.len();
        for (at, tag, what) in [
            (microvm_tag, 2, "a microVM's cost"),
            (microvm_tag + 1, 2, "a VM process's use"),
            (microvm_tag + 1 + 1 + 24, 2, "a microVM's guest time"),
            (second + 4 + 8, 2, "a vCPU's guest time"),
        ] {
            let mut bad = bytes.clone();
            bad[at] = tag;
            assert_eq!(
                StatsAnswer::decode(&bad),
                Err(WireError::Tag { what, tag }),
                "{what}"
            );
        }
        let mut hostile = bytes[..microvm_tag + 1 + 1 + 24 + 1 + 8].to_vec();
        hostile.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(StatsAnswer::decode(&hostile), Err(WireError::Short));
    }

    /// A frame past the run protocol's bound is refused before it is written or read.
    #[test]
    fn frames_are_bounded() {
        assert_eq!(header(1, run::MAX_PAYLOAD as usize + 1), Err(WireError::TooLong));
        let f = frame(to_host::STOP, &[]).unwrap();
        let mut read = &f[..];
        assert_eq!(read_frame(&mut read).unwrap(), Some((to_host::STOP, Vec::new())));
        assert_eq!(read_frame(&mut read).unwrap(), None);
        let cut = &f[..3];
        assert!(read_frame(&mut &cut[..]).is_err());
        let mut long = run::header(1, run::MAX_PAYLOAD).to_vec();
        long[4..8].copy_from_slice(&(run::MAX_PAYLOAD + 1).to_be_bytes());
        assert!(read_frame(&mut &long[..]).is_err());
    }

    /// A run asked of `control`, otherwise as Hex asks for its node VM (joint plan step 39):
    /// its init the run's own command under the nesting grant.
    fn asked(control: Option<ControlAsk>) -> Request {
        Request {
            image: "hex-node@sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            vcpus: 2,
            memory_mib: 2048,
            command: Some(vec!["/init".into()]),
            members: Vec::new(),
            publish: Vec::new(),
            egress: Narrowing::default(),
            shares: Vec::new(),
            nesting: true,
            ready_by: None,
            credentials: Vec::new(),
            control,
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
            upstream_roots: Vec::new(),
        }
    }

    /// Step 11 (b), D252 (Hex's acceptance): the run's control ask crosses the image channel
    /// whole, at its next version, and a run that asks none encodes as one at v13 did but for
    /// its version byte: control is absent, never a default. The controller's key, the socket
    /// grant's directory and both its numbers are read back exactly.
    #[test]
    fn a_runs_control_ask_crosses_the_image_channel_whole() {
        const { assert!(VERSION >= 15, "the channel's version 15 carries the control ask") };
        for control in [
            None,
            Some(ControlAsk {
                controller: [9u8; 32],
                sockets: None,
            }),
            Some(ControlAsk {
                controller: [9u8; 32],
                sockets: Some(SocketGrant {
                    dir: "/run/hex/channel".into(),
                    lanes: 8,
                    window: 65_536,
                }),
            }),
        ] {
            let request = asked(control);
            assert_eq!(Request::decode(&request.encode().unwrap()).unwrap(), request);
        }
        // None asked: one byte past v13's encoding, its absence.
        let none = asked(None).encode().unwrap();
        assert_eq!(none.last(), Some(&0));
    }

    /// Step 11 (b), D252: a started run tells where its endpoint is bound and the microVM's own
    /// key, which the controller pins; a run asked for no endpoint tells none.
    #[test]
    fn a_started_run_tells_where_its_endpoint_is_and_its_vms_key() {
        for control in [
            None,
            Some(ControlAt {
                address: "127.0.0.1:40123".parse().unwrap(),
                vm: [3u8; 32],
            }),
            Some(ControlAt {
                address: "[::1]:40123".parse().unwrap(),
                vm: [4u8; 32],
            }),
        ] {
            let started = Started {
                members: Vec::new(),
                published: Vec::new(),
                control,
                rate_period: std::time::Duration::from_millis(100),
                from: StartedFrom::Restored,
            };
            assert_eq!(Started::decode(&started.encode().unwrap()).unwrap(), started);
        }
    }

    /// D232 through D231 (D234's `gpus=`): the GPUs a run names cross the image channel in
    /// `--gpu-device`'s grammar, whole and in order; an identity that grammar refuses, or a
    /// selection D232 refuses (one GPU twice), is refused as the request is read.
    #[test]
    fn a_runs_gpus_cross_the_image_channel_in_d232s_grammar() {
        use shards_abi::gpu::PhysicalGpu;
        let gpus = vec![
            PhysicalGpu::Pci {
                domain: 0,
                bus: 1,
                device: 0,
                function: 0,
            },
            PhysicalGpu::MetalRegistry(0x1_0000_02a1),
        ];
        let named = Request {
            gpu_devices: gpus.clone(),
            ..request()
        };
        let bytes = named.encode().unwrap();
        assert_eq!(Request::decode(&bytes).unwrap().gpu_devices, gpus);
        // The last identity's text, "metal:00000001000002a1", before the one member's arguments'
        // count (version 18) and the sign-ins' (version 24), made one D232's grammar refuses.
        let mut unnamed = bytes.clone();
        let at = unnamed.len() - 1 - 4 - 4 - "metal:00000001000002a1".len();
        unnamed[at] = b'x';
        assert_eq!(Request::decode(&unnamed), Err(WireError::Gpu));
        let twice = Request {
            gpu_devices: vec![gpus[1], gpus[1]],
            ..request()
        };
        assert_eq!(Request::decode(&twice.encode().unwrap()), Err(WireError::Gpu));
    }

    /// A run's upstream roots (version 26, D242). Expect: each DER read back, in order; a request
    /// cut anywhere in them refused.
    #[test]
    fn a_runs_upstream_roots_cross_the_channel() {
        let rooted = Request {
            upstream_roots: vec![vec![0x30, 0x01, 0x02], vec![0x30; 300]],
            ..request()
        };
        let bytes = rooted.encode().unwrap();
        assert_eq!(Request::decode(&bytes).unwrap(), rooted);
        for cut in 1..=bytes.len() - request().encode().unwrap().len() {
            assert!(Request::decode(&bytes[..bytes.len() - cut]).is_err(), "cut {cut}");
        }
    }

    /// A run's sign-ins and their words (version 24, D242 step 8). Do: carry a sign-in captured
    /// before, with and without a refresh token's placeholder, and one never captured; each word
    /// of a capture (its tokens, its ID token made inert and its scope where it has them), a
    /// refresh, a revocation and their answers, given and refused each way; cut each, and give a
    /// CLI or a refusal no version has. Expect: each read back exactly; each cut and each unknown
    /// tag refused; no token or placeholder in any `Debug`.
    #[test]
    fn a_runs_sign_ins_and_their_words_cross_the_channel() {
        let captured = |refresh: Option<&str>| Captured {
            access: "sign-ins/claude/acct-1/access".into(),
            access_placeholder: "kiln-placeholder-access-0123".into(),
            refresh_placeholder: refresh.map(Into::into),
        };
        let signed = Request {
            sign_ins: vec![
                SignInGrant {
                    sign_in: "claude/acct-1".into(),
                    cli: Cli::ClaudeCode,
                    toward: vec!["api.anthropic.com".into()],
                    captured: Some(captured(Some("kiln-placeholder-refresh-4567"))),
                },
                SignInGrant {
                    sign_in: "codex/acct-2".into(),
                    cli: Cli::Codex,
                    toward: vec!["chatgpt.com".into(), "api.openai.com".into()],
                    captured: None,
                },
            ],
            ..request()
        };
        let bytes = signed.encode().unwrap();
        assert_eq!(Request::decode(&bytes).unwrap(), signed);
        let unrefreshed = Request {
            sign_ins: vec![SignInGrant {
                captured: Some(captured(None)),
                ..signed.sign_ins[0].clone()
            }],
            ..request()
        };
        assert_eq!(
            Request::decode(&unrefreshed.encode().unwrap()).unwrap(),
            unrefreshed
        );
        for cut in 1..=bytes.len() - request().encode().unwrap().len() {
            assert!(Request::decode(&bytes[..bytes.len() - cut]).is_err(), "cut {cut}");
        }
        // The second sign-in's CLI, its code after its name, made one no version has; the one
        // member's MCP bridge's flag (version 25) after it.
        let mut cli = bytes.clone();
        let at = cli.len() - (4 + "chatgpt.com".len() + 4 + "api.openai.com".len() + 4 + 1 + 1 + 1);
        assert_eq!(cli[at], Cli::Codex.code());
        cli[at] = 9;
        assert!(matches!(Request::decode(&cli), Err(WireError::Tag { .. })));
        let tokens = |refresh: bool, id: bool, scope: bool| SignInTokens {
            access: Secret::from("sk-ant-oat01-access"),
            refresh: refresh.then(|| Secret::from("sk-ant-ort01-refresh")),
            expires_in: refresh.then_some(28800),
            id_token: id.then(|| Secret::from("eyJh.eyJw.shards-placeholder")),
            scope: scope.then(|| "user:inference user:profile".into()),
        };
        for (refresh, id, scope) in [(true, true, true), (false, false, false), (true, false, true)] {
            let ask = CaptureAsk {
                id: 7,
                sign_in: "claude/acct-1".into(),
                tokens: tokens(refresh, id, scope),
            };
            let bytes = ask.encode().unwrap();
            assert_eq!(CaptureAsk::decode(&bytes).unwrap(), ask);
            assert!(CaptureAsk::decode(&bytes[..bytes.len() - 1]).is_err());
            let shown = format!("{ask:?}");
            assert!(!shown.contains("sk-ant") && !shown.contains("eyJ"), "{shown}");
        }
        let refresh = RefreshAsk {
            id: 8,
            sign_in: "claude/acct-1".into(),
        };
        assert_eq!(RefreshAsk::decode(&refresh.encode().unwrap()).unwrap(), refresh);
        let revoked = Revoked {
            sign_in: "claude/acct-1".into(),
        };
        assert_eq!(Revoked::decode(&revoked.encode().unwrap()).unwrap(), revoked);
        let refusals = [
            SignInRefusal::Denied("no such sign-in".into()),
            SignInRefusal::Unavailable("the broker is down".into()),
            SignInRefusal::Ended,
            SignInRefusal::Unanswered,
        ];
        for result in [
            Ok(captured(Some("kiln-placeholder-refresh-4567"))),
            Ok(captured(None)),
        ]
        .into_iter()
        .chain(refusals.iter().cloned().map(Err))
        {
            let answer = CaptureAnswer { id: 9, result };
            let bytes = answer.encode().unwrap();
            assert_eq!(CaptureAnswer::decode(&bytes).unwrap(), answer);
            assert!(CaptureAnswer::decode(&bytes[..bytes.len() - 1]).is_err());
            let shown = format!("{answer:?}");
            assert!(!shown.contains("kiln-placeholder"), "{shown}");
        }
        for result in [Ok(Secret::from(r#"{"access_token":"sk-ant-oat01-rotated"}"#))]
            .into_iter()
            .chain(refusals.iter().cloned().map(Err))
        {
            let answer = RefreshAnswer { id: 10, result };
            let bytes = answer.encode().unwrap();
            assert_eq!(RefreshAnswer::decode(&bytes).unwrap(), answer);
            assert!(RefreshAnswer::decode(&bytes[..bytes.len() - 1]).is_err());
            assert!(!format!("{answer:?}").contains("sk-ant"));
        }
        // A refusal's tag no version has.
        let mut bad = CaptureAnswer {
            id: 9,
            result: Err(SignInRefusal::Ended),
        }
        .encode()
        .unwrap();
        *bad.last_mut().unwrap() = 9;
        assert!(matches!(CaptureAnswer::decode(&bad), Err(WireError::Tag { .. })));
    }
}
