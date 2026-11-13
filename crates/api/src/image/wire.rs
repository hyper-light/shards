//! The channel between an image boot ([`super::ImageShards`]) and its run's host, `shards
//! api-run` (D231): frames on the run host's stdin and stdout with the run protocol's header
//! (a kind, three zero bytes, a big-endian u32 length; `shards_abi::run::header`), each
//! payload at most `shards_abi::run::MAX_PAYLOAD`. Integers are big-endian; a list is a u32
//! count then its items; bytes and text are a u32 length then themselves, text UTF-8. A
//! payload is read whole or refused: a field past its end, bytes after its last field, an
//! unknown tag or a bad UTF-8 string is a [`WireError`], never a default.
//!
//! Credentials ride only in [`Request`], each in a [`Secret`], whose `Debug` prints its
//! length alone.

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
/// no kind for.
pub const VERSION: u8 = 13;

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
        }
    }
}

impl std::error::Error for WireError {}

/// Bytes a caller would not have printed: a credential, as often as not. Its `Debug` says
/// how many there are, never what.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(pub Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
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

/// A credential grant by reference (D242): the credential named `reference` (a secret's
/// path in the orchestrator's store, never a value) may be filled in at the VM's egress
/// toward the hosts in `toward`, and no others. The grant carries no value, so it may
/// print; the value rides a lease the injector pulls just in time, never this channel.
///
/// Each host in `toward` is a DNS name lowered, as a network's `--egress=HOST:PORT` names a
/// destination's host by name (D236), or [`host.docker.internal`](shards_dns::HOST), the
/// host itself; never an address (TLS names a server by its DNS name alone, RFC 6066 §3),
/// never with a port, and never `localhost`. The orchestrator's side checks that where it builds the request
/// ([`super::request`]); a side reading the wire takes them as bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CredentialGrant {
    pub reference: String,
    pub toward: Vec<String>,
}

/// What the credential injector (a gateway, not a proxy: RFC 9110 §3.7, so it may set the
/// `Authorization` header a proxy may not, §11.6.2) does with a request it has terminated,
/// decided by [`decide`] from the TLS name the flow arrived on, the request's host and the
/// VM's grants (D242). The injector renders the outcome; this enum is the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fill {
    /// Fill the credential named `reference` into the request, then forward it.
    With(String),
    /// No grant names this host: forward the request with no credential (default deny).
    Pass,
    /// The request's host disagrees with the TLS name the flow arrived on, so the flow is
    /// misdirected (RFC 9110 §7.4): refuse it 421, never guessing which host was meant.
    Misdirected,
}

/// Decide what the injector fills into a request to a credential-granted host (D242).
/// `sni` is the TLS server name the flow was terminated on, `host` the request's host (its
/// HTTP/1.1 `Host` or HTTP/2 `:authority`, the authority alone, without a port); `grants`
/// are the VM's. A value is filled only when the SNI, the host and a grant's host all
/// agree: the SNI alone is not enough (a client picks its `Host` freely; domain fronting),
/// so a `Host` that differs from the SNI is [`Misdirected`](Fill::Misdirected), and
/// otherwise the first grant whose `toward` names the host fills it, else
/// [`Pass`](Fill::Pass). Matching is ASCII case-insensitive, as DNS names compare (RFC
/// 9549 §2.3); callers that also require the request to carry the credential's placeholder
/// add that where they substitute.
pub fn decide(grants: &[CredentialGrant], sni: &str, host: &str) -> Fill {
    if !sni.eq_ignore_ascii_case(host) {
        return Fill::Misdirected;
    }
    for grant in grants {
        if grant.toward.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            return Fill::With(grant.reference.clone());
        }
    }
    Fill::Pass
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
}

/// A member's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub member: u32,
    pub channel: Channel,
    pub bytes: Vec<u8>,
}

/// Bytes for a member's stdin, at most the room its host has given and the orchestrator has
/// not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub member: u32,
    pub bytes: Vec<u8>,
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

/// What a VM process uses of its host, as the host counts it (`platform::process_usage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostUse {
    /// Its CPU time, user and system together, in ns: its vCPUs' guest time among it.
    pub cpu_ns: u64,
    pub resident_bytes: u64,
    /// Its own memory, as memory per VM is counted (PM M30): macOS's physical footprint,
    /// Linux's anonymous resident pages, Windows' private working set.
    pub footprint_bytes: u64,
}

/// What a microVM's vCPUs have run of its guest, as its hypervisor counts it: each by index,
/// and what those it no longer has ran, which stay its machine's (guest-cpu-time-v1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuestTimes {
    /// In index order, each index once.
    pub vcpus: Vec<VcpuTime>,
    pub departed_ns: u64,
}

impl GuestTimes {
    /// Its machine's: what every vCPU it has had ran of its guest.
    pub fn total_ns(&self) -> u64 {
        self.vcpus
            .iter()
            .fold(self.departed_ns, |total, v| total.saturating_add(v.guest_ns))
    }
}

/// One vCPU's guest time, as a sample read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuTime {
    pub index: u32,
    /// Nanoseconds, never less than an earlier sample of the same vCPU said.
    pub guest_ns: u64,
    /// Why this sample did not read it: `guest_ns` is then the most it read before.
    pub unread: Option<String>,
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

/// How the run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// Its own command, or `ATTEND`'s holder, ended with this status.
    Exited(u32),
    /// `STOP`: its VM killed at once.
    Stopped,
    /// Its VM, its host or its guest failed.
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
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(len as usize)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::OutOfMemory, "a frame's payload"))?;
    payload.resize(len as usize, 0);
    from.read_exact(&mut payload)?;
    Ok(Some((kind, payload)))
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
                    self.text(why)?;
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
                        1 => Some(self.text()?),
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
        // directory's flag, its input's and its terminal's.
        let n = r.count(1 + 4 + 4 + 4 + 1 + 1 + 1)?;
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
                // Read with the members' working directories, inputs and terminals, appended
                // last.
                workdir: None,
                input: false,
                terminal: false,
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
            credentials.push(CredentialGrant { reference, toward });
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
        })
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
        r.end()?;
        Ok(Started { members, published })
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
            bytes: bytes.to_vec(),
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

    /// The injector fills a credential only when the TLS name, the request's host and a
    /// grant's host all agree (D242): a grant toward one host does not fill a request to
    /// another, a host that differs from the SNI is misdirected, and matching ignores
    /// ASCII case.
    #[test]
    fn decide_fills_only_on_an_agreed_granted_host() {
        let grants = vec![
            CredentialGrant {
                reference: "anthropic/claude-max".into(),
                toward: vec!["api.anthropic.com".into(), "platform.claude.com".into()],
            },
            CredentialGrant {
                reference: "openai/chatgpt".into(),
                toward: vec!["chatgpt.com".into()],
            },
        ];
        // SNI == Host == a grant's host: filled, the first grant that names it.
        assert_eq!(
            decide(&grants, "api.anthropic.com", "api.anthropic.com"),
            Fill::With("anthropic/claude-max".into())
        );
        assert_eq!(
            decide(&grants, "chatgpt.com", "chatgpt.com"),
            Fill::With("openai/chatgpt".into())
        );
        // Case folds, as DNS names compare (RFC 9549 §2.3).
        assert_eq!(
            decide(&grants, "API.Anthropic.Com", "api.anthropic.com"),
            Fill::With("anthropic/claude-max".into())
        );
        // A host no grant names: forwarded bare (default deny).
        assert_eq!(decide(&grants, "example.com", "example.com"), Fill::Pass);
        assert_eq!(decide(&[], "api.anthropic.com", "api.anthropic.com"), Fill::Pass);
        // A grant toward host A does not fill a request whose host is B.
        assert_eq!(decide(&grants, "evil.example", "evil.example"), Fill::Pass);
        // The request's host disagrees with the TLS name it arrived on: misdirected, and
        // never filled even though the SNI is a granted host (the SNI alone is not enough).
        assert_eq!(
            decide(&grants, "api.anthropic.com", "evil.example"),
            Fill::Misdirected
        );
        assert_eq!(
            decide(&grants, "chatgpt.com", "api.anthropic.com"),
            Fill::Misdirected
        );
    }

    /// A credential's value never prints: a [`Secret`]'s `Debug` says only its length, and
    /// a [`CredentialGrant`] carries no value to print (reference and hosts alone).
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
        };
        let shown = format!("{grant:?}");
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
        };
        let shown = format!("{staged:?}");
        assert!(
            !shown.contains("DEADBEEF"),
            "a staged value leaked into Debug: {shown}"
        );
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
        }
    }

    /// Every message reads back as written, its fields at their edges.
    #[test]
    fn messages_round_trip() {
        let r = request();
        assert_eq!(Request::decode(&r.encode().unwrap()).unwrap(), r);
        // The run's credential grants (version 8, D242): none, one with several hosts, and
        // one with none, each reading back as written.
        let mut credited = request();
        credited.credentials = vec![
            CredentialGrant {
                reference: "anthropic/claude-max".into(),
                toward: vec!["api.anthropic.com".into(), "platform.claude.com".into()],
            },
            CredentialGrant {
                reference: "openai/chatgpt".into(),
                toward: Vec::new(),
            },
        ];
        assert_eq!(Request::decode(&credited.encode().unwrap()).unwrap(), credited);
        // Each member's working directory, or none, in the members' order (version 5), and
        // whether its input is the orchestrator's (version 6).
        let mut worked = request();
        worked.members[0].workdir = Some("/Users/ada/proj".into());
        worked.members.push(MemberStage {
            kind: MemberKind::Agent,
            name: "helper".into(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            input: false,
            terminal: false,
        });
        worked.members.push(MemberStage {
            workdir: Some("/".into()),
            input: true,
            terminal: true,
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
        assert_eq!(
            asked.get(asked.len() - 19..).unwrap(),
            [1, 1, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 1, 0, 0, 0, 0, 0, 0],
            "appended: the grant's flag, the deadline's, the destinations kept (all), the one \
             member's working directory (none) and its input, the credential grants' count \
             (zero, version 8), the member's terminal (none, version 11), then whether the \
             members are held apart (no, version 12)"
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
                bytes: b"\0\xff\x80{\"id\":1}\nno-newline".to_vec()
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
        // One member, so its input is the seventh byte from the end (the count's four, its
        // terminal's one and the reach's flag follow it).
        let mut bad = request().encode().unwrap();
        let at = bad.len() - 7;
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
        // The reach's flag, last where none is held apart.
        let mut flag = held(None).encode().unwrap();
        let at = flag.len() - 1;
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
        let at = kindless.len() - (1 + 4 + "helper".len() + 4);
        assert_eq!(kindless[at], MemberKind::Agent as u8);
        kindless[at] = 2;
        assert_eq!(
            Request::decode(&kindless),
            Err(WireError::Tag {
                what: "a member's kind",
                tag: 2
            })
        );
        let mut hostile = held(None).encode().unwrap();
        hostile.pop();
        hostile.push(1);
        hostile.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(Request::decode(&hostile), Err(WireError::Short));
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
        // them defaulted. The request's one member makes each of those one byte.
        let mut v1 = request().encode().unwrap();
        v1.truncate(v1.len() - 11);
        v1[0] = 1;
        assert_eq!(Request::decode(&v1), Err(WireError::Version(1)));
        let mut v4 = request().encode().unwrap();
        v4.truncate(v4.len() - 8);
        v4[0] = 4;
        assert_eq!(Request::decode(&v4), Err(WireError::Version(4)));
        let mut v5 = request().encode().unwrap();
        v5.truncate(v5.len() - 7);
        v5[0] = 5;
        assert_eq!(Request::decode(&v5), Err(WireError::Version(5)));
        // One of version 6 carries every field version 7 does, but its host ends the run at
        // a room: refused by its version, before any room is told (D241).
        let mut v6 = request().encode().unwrap();
        v6.truncate(v6.len() - 6);
        v6[0] = 6;
        assert_eq!(Request::decode(&v6), Err(WireError::Version(6)));
        // One of version 7, without the credential grants' count: refused by its version
        // (D242), so an older reader never reads a grant with its field defaulted.
        let mut v7 = request().encode().unwrap();
        v7.truncate(v7.len() - 6);
        v7[0] = 7;
        assert_eq!(Request::decode(&v7), Err(WireError::Version(7)));
        // One of version 8, without its members' terminals: refused by its version (D244), so
        // an older reader never starts a member with pipes where its owner asked a terminal.
        let mut v8 = request().encode().unwrap();
        v8.truncate(v8.len() - 2);
        v8[0] = 8;
        assert_eq!(Request::decode(&v8), Err(WireError::Version(8)));
        // One of version 11, without the members' reach: refused by its version (D246), so a
        // run host never takes a request that holds its members apart as one that holds none.
        let mut v11 = request().encode().unwrap();
        v11.truncate(v11.len() - 1);
        v11[0] = 11;
        assert_eq!(Request::decode(&v11), Err(WireError::Version(11)));
        // One of version 12 carries every field version 13 does, but its host ends the run at
        // a rate: refused by its version, before any rate is told (D247).
        let mut v12 = request().encode().unwrap();
        v12[0] = 12;
        assert_eq!(Request::decode(&v12), Err(WireError::Version(12)));
        // The nesting flag, before the deadline's tag, the destinations', the one member's
        // working directory's and input's, the credential grants' count (version 8), the
        // member's terminal's (version 11) and the members' reach (version 12): no flag.
        let mut flag = request().encode().unwrap();
        let at = flag.len() - 11;
        flag[at] = 2;
        assert_eq!(
            Request::decode(&flag),
            Err(WireError::Tag {
                what: "a flag",
                tag: 2
            })
        );
        // The destinations' list, before the member's working directory and input, the
        // credential grants' count, the member's terminal and the members' reach: no list of
        // version 3's and on.
        let mut list = request().encode().unwrap();
        let at = list.len() - 9;
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
        let at = bad.len() - 2;
        assert_eq!(
            bad[at], 0,
            "the member's terminal flag precedes the members' reach"
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
                    unread: Some("its vCPU had left its guest".into()),
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
}
