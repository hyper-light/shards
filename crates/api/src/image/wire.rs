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

use shards_abi::run;
pub use shards_abi::run::nesting::Ready;

/// The channel's version, the first byte of [`Request`]: changed only by appending. 2
/// appends the nesting grant's consent and deadline (D234); 3, the destinations kept
/// ([`Narrowing::destinations`], D236), and to each published port in [`Started`], the
/// members it reaches; 4, a host directory plugged into a running member and let go of
/// (`PLUG`, `UNPLUG`, answered `PLUGGED`; D235), so that a side that has no plugs refuses
/// the run as it is asked, before any plug is.
pub const VERSION: u8 = 4;

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
}

impl Default for Narrowing {
    fn default() -> Self {
        Narrowing {
            egress: Keep::All,
            mcp: Keep::All,
            destinations: Keep::All,
            dns: true,
        }
    }
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

/// What init's `STATS` says, each line `None` where it said none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
        let n = r.count(1 + 4 + 4 + 4)?;
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
        let egress = Narrowing {
            egress,
            mcp,
            destinations: r.keep()?,
            dns,
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
    /// Its id, then 0 and init's lines as it said them, or 1 and why none came.
    pub fn encode(id: u32, result: Result<&[u8], &str>) -> Result<Vec<u8>, WireError> {
        let mut w = Writer(Vec::new());
        w.u32(id);
        match result {
            Ok(lines) => {
                w.u8(0);
                w.bytes(lines)?;
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
            0 => Ok(Stats::parse(&String::from_utf8_lossy(r.bytes()?))),
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
            },
            shares: vec![ShareAt {
                host: "/home/u/work".into(),
                at: "/work".into(),
                read_only: false,
            }],
            nesting: false,
            ready_by: None,
        }
    }

    /// Every message reads back as written, its fields at their edges.
    #[test]
    fn messages_round_trip() {
        let r = request();
        assert_eq!(Request::decode(&r.encode().unwrap()).unwrap(), r);
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
            asked.get(asked.len() - 11..).unwrap(),
            [1, 1, 1, 2, 3, 4, 5, 6, 7, 8, 0],
            "appended: the grant's flag, the deadline's, then the destinations kept (all)"
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
        let answer = StatsAnswer::decode(&StatsAnswer::encode(4, Ok(b"cpu 10\npids 3\n")).unwrap()).unwrap();
        assert_eq!(answer.id, 4);
        let stats = answer.result.unwrap();
        assert_eq!(
            (stats.cpu_ns, stats.pids, stats.memory_bytes),
            (Some(10), Some(3), None)
        );
        let refusal = StatsAnswer::decode(&StatsAnswer::encode(5, Err("no member 9 runs")).unwrap()).unwrap();
        assert_eq!(refusal.result, Err("no member 9 runs".into()));
    }

    fn libc_eio() -> i32 {
        5
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

    /// Unknown versions and tags are refused, never defaulted.
    #[test]
    fn unknown_versions_and_tags_are_refused() {
        let mut bytes = request().encode().unwrap();
        bytes[0] = VERSION + 1;
        assert_eq!(Request::decode(&bytes), Err(WireError::Version(VERSION + 1)));
        // A request of version 1, without the nesting grant's fields, is no request of
        // this one's: refused by its version, never read with the grant defaulted.
        let mut v1 = request().encode().unwrap();
        v1.truncate(v1.len() - 2);
        v1[0] = 1;
        assert_eq!(Request::decode(&v1), Err(WireError::Version(1)));
        // The nesting flag, before the deadline's tag and the destinations': no flag.
        let mut flag = request().encode().unwrap();
        let at = flag.len() - 3;
        flag[at] = 2;
        assert_eq!(
            Request::decode(&flag),
            Err(WireError::Tag {
                what: "a flag",
                tag: 2
            })
        );
        // The destinations' list, last: no list of version 3's.
        let mut list = request().encode().unwrap();
        if let Some(last) = list.last_mut() {
            *last = 2;
        }
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
