//! Exec lanes (plan step 11, D238; ROOT's design (B), 2026-11-13): a command run in one of a
//! microVM's members is a lane of its own, which asks for it, steers it and hears it end, and
//! its stdout, stderr and stdin are step 5's streams on lanes of their own, as a member's are.
//!
//! - **The ask.** An exec lane's first frame (`Tag::MemberExec`) names the run, its
//!   generation and the member, then carries step 6's member exec as the run protocol carries
//!   it, byte for byte: a `run::Spec` (D243). Its length is the run protocol's own bound, as
//!   large as Linux execs (`run::MAX_PAYLOAD`).
//! - **Its streams.** The endpoint numbers each exec, and its stdout, stderr (none on a
//!   terminal) and stdin (where it reads one) are step 5's streams under a run of their own
//!   ([`exec_run`]): the run's identity and the exec's number, hashed, so that the
//!   controller's subscriptions name them as they name a member's.
//! - **Its steering.** After `Started`, the lane carries the controller's resizes and
//!   signals, D180's frames (`Tag::Resize`, `Tag::Signal`), one in flight to the endpoint's
//!   host at a time, as step 5 has one record in flight; the endpoint tells its exit
//!   (`Tag::Exit`) and finishes it.
//! - **Its host.** The endpoint starts nothing itself: it hands its host the ask and its
//!   streams' handles ([`Start`]), and the host, which charges the run's budget for each exec,
//!   says how it went ([`Replies`]).
//! - **An MCP connection** (D254) is an exec in all but its start: its lane's first frame is
//!   `Tag::Mcp`, the scope alone ([`Kind::Mcp`]), and its host starts it as the member's harness
//!   connects to its MCP bridge, the harness's bytes its stdout and the controller's its stdin.
//! - **A member's own lane** (D238, goal 58's steps 5 and 6): its first frame is `Tag::Member`,
//!   the scope alone ([`Kind::Member`]); it makes no stream, as the member's own are step 5's
//!   already. Its host ([`MemberStart`]) says the member runs (`Running`) or ended (its
//!   `Exit`), answers each steer, a signal to its command or a resize of its own terminal,
//!   taken or refused, typed (`Steered`, [`Unsteered`]), one in flight, and tells its end as
//!   it comes, typed ([`End`]): its command's exit (`Exit`), or the signal that killed it
//!   (`Signaled`), which a status of 128 plus its number cannot tell from an exit.
//! - **A log lane** (D238's log lane): its first frame is `Tag::Log`, the scope and a
//!   [`LogAsk`]: the run's command's log or a member's, its channels, where each starts and
//!   whether it is followed ([`Kind::Log`]). It is an exec in all but its start: its host
//!   reads the log the run keeps (D161, D240) into one stream of step 5's records, each a
//!   [`LogRecord`] at its own place in the log, and says it exited once all it was to read is
//!   consumed; or why it reads none, typed ([`Unlogged`]).

use std::mem::size_of;
use std::sync::mpsc;

use sha2::{Digest as _, Sha256};
use shards_abi::run::managed::Channel;
use shards_abi::run::{self, Size, Spec};

use crate::frame::{self, Tag};
use crate::managed::{self, Producer, Reason, RunId, input};
use crate::net::Sender;

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
/// An ask's scope before its spec: the run (32), its generation (u64) and the member (u32).
const SCOPE: usize = size_of::<RunId>() + size_of::<u64>() + size_of::<u32>();
/// The most an ask's frame carries: its scope and the largest spec the run protocol carries,
/// as large as Linux execs (`run::MAX_PAYLOAD`).
pub const ASK_MOST: usize = SCOPE + run::MAX_PAYLOAD as usize;

/// What a controller asks of a member (R04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Step 6's member exec: its argv, environment, working directory, terminal and stdin. Its
    /// user and setup are the member's own (D243): the host refuses others.
    Exec(Box<Spec>),
    /// The next connection the member's harness makes to its MCP bridge (D254): the harness's
    /// bytes its stdout, the controller's its stdin, no stderr and no steering.
    Mcp,
    /// The member itself: whether it runs or how it ended, its end as it comes, and its
    /// steering, each steer answered ([`MemberStart`]).
    Member,
    /// The log its run keeps of the run's command's output or a member's (D238's log lane): its
    /// records as one stream, no stdin and no steering ([`LogAsk`]).
    Log(LogAsk),
}

/// What a controller asks for in a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub run: RunId,
    pub generation: u64,
    /// The member asked of; a log ask's source's ([`LogSource::member`]).
    pub member: u32,
    pub kind: Kind,
}

impl Ask {
    /// A log lane's ask in `run`'s generation `generation`, its member its source's.
    pub fn log(run: RunId, generation: u64, log: LogAsk) -> Ask {
        Ask {
            run,
            generation,
            member: log.source.member(),
            kind: Kind::Log(log),
        }
    }

    /// Its first frame, whole: `Tag::MemberExec` and its spec, `Tag::Mcp` or `Tag::Member` and
    /// its scope alone, or `Tag::Log` and its [`LogAsk`]. A log ask whose member is not its
    /// source's is refused.
    pub fn frame(&self) -> Result<Vec<u8>, managed::Error> {
        let (tag, spec, log) = match &self.kind {
            Kind::Exec(spec) => (Tag::MemberExec, Some(spec.as_ref()), None),
            Kind::Mcp => (Tag::Mcp, None, None),
            Kind::Member => (Tag::Member, None, None),
            Kind::Log(log) if log.source.member() == self.member => (Tag::Log, None, Some(log.encoded())),
            Kind::Log(_) => return Err(managed::Error::Protocol(managed::ProtocolError::Scope)),
        };
        let more = spec
            .and_then(Spec::encoded_len)
            .unwrap_or(0)
            .saturating_add(log.map_or(0, |l| l.len()));
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(SCOPE.saturating_add(more))
            .map_err(|_| managed::Error::Admission(managed::AdmissionError::Allocation))?;
        payload.extend_from_slice(&self.run.0);
        payload.extend_from_slice(&self.generation.to_be_bytes());
        payload.extend_from_slice(&self.member.to_be_bytes());
        if let Some(spec) = spec {
            spec.encode_into(&mut payload);
        }
        if let Some(log) = log {
            payload.extend_from_slice(&log);
        }
        if payload.len() > ASK_MOST {
            return Err(managed::Error::Protocol(managed::ProtocolError::Length));
        }
        let mut out = Vec::new();
        frame::put(&mut out, tag, &payload)?;
        Ok(out)
    }

    /// The ask a whole first frame carries, its head and payload: a protocol error for one cut
    /// short, a spec the run protocol would not take, an MCP or member ask that carries more
    /// than its scope, or a log ask that is no [`LogAsk`].
    pub fn decode(whole: &[u8]) -> Result<Ask, Reason> {
        let (tag, len) = frame::peek(whole)
            .map_err(|_| Reason::Protocol)?
            .ok_or(Reason::Protocol)?;
        let payload = whole
            .get(FRAME_HEAD..)
            .filter(|p| p.len() == len)
            .ok_or(Reason::Protocol)?;
        let (run, rest) = payload.split_first_chunk::<32>().ok_or(Reason::Protocol)?;
        let (generation, rest) = rest.split_first_chunk::<8>().ok_or(Reason::Protocol)?;
        let (member, rest) = rest.split_first_chunk::<4>().ok_or(Reason::Protocol)?;
        let member = u32::from_be_bytes(*member);
        let kind = match tag {
            Tag::MemberExec => Kind::Exec(Box::new(Spec::decode(rest).ok_or(Reason::Protocol)?)),
            Tag::Mcp if rest.is_empty() => Kind::Mcp,
            Tag::Member if rest.is_empty() => Kind::Member,
            Tag::Log => Kind::Log(LogAsk::decode(member, rest)?),
            _ => return Err(Reason::Protocol),
        };
        Ok(Ask {
            run: RunId(*run),
            generation: u64::from_be_bytes(*generation),
            member,
            kind,
        })
    }
}

/// Whose output a log lane reads: the run's own command's, as its log keeps it (D161), or a
/// member's, as the daemon keeps it in `members/<index>` (D240).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Command,
    Member(u32),
}

impl LogSource {
    /// The member its ask names: its own index, or 0 for the run's command, whose ask says it
    /// is the command's.
    pub fn member(self) -> u32 {
        match self {
            LogSource::Command => 0,
            LogSource::Member(member) => member,
        }
    }
}

/// Which channels of its source a log lane reads: one at least.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogChannels {
    pub stdout: bool,
    pub stderr: bool,
}

impl LogChannels {
    /// Both, as `docker logs` shows a run's.
    pub const BOTH: LogChannels = LogChannels {
        stdout: true,
        stderr: true,
    };

    /// Whether it reads `channel`.
    pub fn reads(self, channel: Channel) -> bool {
        match channel {
            Channel::Stdout => self.stdout,
            Channel::Stderr => self.stderr,
        }
    }

    /// Its byte: bit 0 stdout, bit 1 stderr.
    fn bits(self) -> u8 {
        u8::from(self.stdout) | (u8::from(self.stderr) << 1)
    }

    /// The channels `bits` names: none for no channel, or a bit no channel has.
    fn from_bits(bits: u8) -> Option<LogChannels> {
        match bits {
            1..=3 => Some(LogChannels {
                stdout: bits & 1 != 0,
                stderr: bits & 2 != 0,
            }),
            _ => None,
        }
    }
}

/// A place in a log, as its store keeps it (the daemon's `logs::Pos`, D163's cursor): its
/// segment, the number of its record in that segment, and a byte of that record's output.
/// Places order as the log does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LogPos {
    pub seq: u64,
    pub no: u64,
    pub byte: u64,
}

impl LogPos {
    /// Its bytes: its three numbers, big-endian, as a shipper keeps its place (D163).
    pub const BYTES: usize = 3 * size_of::<u64>();

    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0; Self::BYTES];
        for (field, n) in out
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip([self.seq, self.no, self.byte])
        {
            field.copy_from_slice(&n.to_be_bytes());
        }
        out
    }

    pub fn from_bytes(bytes: &[u8; Self::BYTES]) -> LogPos {
        let [seq, no, byte] = [0, 1, 2].map(|i| {
            bytes
                .as_chunks::<8>()
                .0
                .get(i)
                .map_or(0, |field| u64::from_be_bytes(*field))
        });
        LogPos { seq, no, byte }
    }

    /// The place `n` bytes on in its record: where a lane that had the bytes before it goes on.
    pub fn after(self, n: u64) -> Option<LogPos> {
        Some(LogPos {
            byte: self.byte.checked_add(n)?,
            ..self
        })
    }

    /// The first place of the record after its own.
    pub fn next_record(self) -> Option<LogPos> {
        Some(LogPos {
            seq: self.seq,
            no: self.no.checked_add(1)?,
            byte: 0,
        })
    }
}

/// Where a log lane starts, each channel apart: the first place of it its asker has not had,
/// or, where none is given, the oldest its log keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogCursor {
    pub stdout: Option<LogPos>,
    pub stderr: Option<LogPos>,
}

impl LogCursor {
    /// From the oldest kept, each channel.
    pub const OLDEST: LogCursor = LogCursor {
        stdout: None,
        stderr: None,
    };

    /// `channel`'s place.
    pub fn of(self, channel: Channel) -> Option<LogPos> {
        match channel {
            Channel::Stdout => self.stdout,
            Channel::Stderr => self.stderr,
        }
    }

    /// The cursor past `record` for the channels `read` reads: each read channel's place moved
    /// on to where a lane that had `record` goes on, never back. The log is read in its order,
    /// so a read channel has had all of itself before `record` too; a channel not read keeps
    /// its place.
    pub fn past(self, record: &LogRecord, read: LogChannels) -> LogCursor {
        let Some(after) = record.after() else {
            return self;
        };
        let on = |at: Option<LogPos>, channel| match read.reads(channel) {
            true => Some(at.map_or(after, |at| at.max(after))),
            false => at,
        };
        LogCursor {
            stdout: on(self.stdout, Channel::Stdout),
            stderr: on(self.stderr, Channel::Stderr),
        }
    }
}

/// A log lane's ask past its scope: whose log, which channels, from where, and whether it is
/// followed, its lane ending at its source's end, or read as kept now, its lane ending once that
/// is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogAsk {
    pub source: LogSource,
    pub channels: LogChannels,
    pub cursor: LogCursor,
    pub follow: bool,
}

/// A log ask past its scope: its source (0 the run's command, 1 a member), its channels' bits,
/// whether it follows, and each channel's place (0 and no place, or 1 and its place).
const LOG_ASK: usize = 3 * size_of::<u8>() + 2 * (size_of::<u8>() + LogPos::BYTES);

impl LogAsk {
    fn encoded(&self) -> [u8; LOG_ASK] {
        let mut out = [0; LOG_ASK];
        let source = u8::from(matches!(self.source, LogSource::Member(_)));
        let place = |at: Option<LogPos>| {
            let mut field = [0; 1 + LogPos::BYTES];
            if let (Some(at), Some((flag, pos))) = (at, field.split_first_mut()) {
                *flag = 1;
                pos.copy_from_slice(&at.to_bytes());
            }
            field
        };
        let (stdout, stderr) = (place(self.cursor.stdout), place(self.cursor.stderr));
        let head = [source, self.channels.bits(), u8::from(self.follow)];
        for (slot, byte) in out.iter_mut().zip(head.iter().chain(&stdout).chain(&stderr)) {
            *slot = *byte;
        }
        out
    }

    /// The log ask `rest` carries for `member`: a protocol error for any other length, source,
    /// channels, follow or place, a command's ask naming a member, or a place's bytes past none.
    fn decode(member: u32, rest: &[u8]) -> Result<LogAsk, Reason> {
        let rest = <&[u8; LOG_ASK]>::try_from(rest).map_err(|_| Reason::Protocol)?;
        let ([source, channels, follow], places) = rest.split_first_chunk::<3>().ok_or(Reason::Protocol)?;
        let (stdout, stderr) = places
            .split_first_chunk::<{ 1 + LogPos::BYTES }>()
            .ok_or(Reason::Protocol)?;
        let source = match (*source, member) {
            (0, 0) => LogSource::Command,
            (1, member) => LogSource::Member(member),
            _ => return Err(Reason::Protocol),
        };
        let place = |field: &[u8]| -> Result<Option<LogPos>, Reason> {
            let (flag, pos) = field.split_first().ok_or(Reason::Protocol)?;
            let pos = <&[u8; LogPos::BYTES]>::try_from(pos).map_err(|_| Reason::Protocol)?;
            match *flag {
                0 if pos.iter().all(|b| *b == 0) => Ok(None),
                1 => Ok(Some(LogPos::from_bytes(pos))),
                _ => Err(Reason::Protocol),
            }
        };
        Ok(LogAsk {
            source,
            channels: LogChannels::from_bits(*channels).ok_or(Reason::Protocol)?,
            cursor: LogCursor {
                stdout: place(stdout.as_slice())?,
                stderr: place(stderr)?,
            },
            follow: match *follow {
                0 => false,
                1 => true,
                _ => return Err(Reason::Protocol),
            },
        })
    }
}

/// Why a log lane reads nothing, typed: what its controller matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlogged {
    /// A place it was asked from is older than the oldest its log keeps, `oldest`: what came
    /// between went with the segments its retention let go (D161, D186).
    Rotated { oldest: LogPos },
    /// The run keeps no log (`--log-driver none`, D161), nor any of its members' (D240).
    Unkept,
    /// The run's command's log is kept from reading: its driver sends it elsewhere and keeps no
    /// copy (`--log-opt cache-disabled=true`, D163), as `logs` refuses it.
    Uncached,
    /// The run has no member of that index.
    Unknown,
}

/// [`Unlogged`]'s codes, past [`Unsteered`]'s 19 to 22: `Rotated` has a frame of its own
/// (`Tag::Rotated`), as it names a place.
const UNKEPT: u32 = 23;
const UNCACHED: u32 = 24;
const UNKNOWN: u32 = 25;

impl Unlogged {
    /// The refusal of `code`, of those a code says.
    fn of(code: u32) -> Option<Unlogged> {
        match code {
            UNKEPT => Some(Unlogged::Unkept),
            UNCACHED => Some(Unlogged::Uncached),
            UNKNOWN => Some(Unlogged::Unknown),
            _ => None,
        }
    }
}

/// Why a log's output was not kept at a place, as its gap says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Gapped {
    /// Let go so that its log never held up its member's owner: its keeping fell behind (D240).
    Behind = 1,
    /// Its write failed: its disk took no more.
    Unwritten = 2,
}

impl TryFrom<u8> for Gapped {
    type Error = ();
    fn try_from(byte: u8) -> Result<Self, ()> {
        match byte {
            1 => Ok(Gapped::Behind),
            2 => Ok(Gapped::Unwritten),
            _ => Err(()),
        }
    }
}

/// What one of a log lane's records says: each of step 5's records its host submits carries
/// one, in its log's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogRecord {
    /// Bytes of `channel`'s output as its log keeps them, the first at `pos`, come at `at`
    /// (nanoseconds since the Unix epoch, as its log stamps them).
    Output {
        channel: Channel,
        pos: LogPos,
        at: u64,
        bytes: Vec<u8>,
    },
    /// `lost` bytes of `channel`'s output its log did not keep there, typed by why, said at
    /// `at`: the gap's own record at `pos`.
    Gap {
        channel: Channel,
        pos: LogPos,
        at: u64,
        lost: u64,
        why: Gapped,
    },
    /// The log from where the lane had read to `oldest` went with segments its retention let go
    /// before the lane read them: the lane goes on from `oldest`.
    Rotated { oldest: LogPos },
}

/// A record's kinds, its first byte.
const OUTPUT: u8 = 1;
const GAP: u8 = 2;
const ROTATED: u8 = 3;

/// An output record's bytes before its output: its kind, its channel, its place and its time.
/// What of a lane's record its output has room for is the record's size less this.
pub const LOG_OUTPUT_HEAD: usize = 2 * size_of::<u8>() + LogPos::BYTES + size_of::<u64>();
/// A gap's record whole: an output record's head, then the bytes lost and why.
const GAP_BYTES: usize = LOG_OUTPUT_HEAD + size_of::<u64>() + size_of::<u8>();
/// A rotation's record whole: its kind and the oldest place kept.
const ROTATED_BYTES: usize = size_of::<u8>() + LogPos::BYTES;
/// The most a record other than output takes: what a lane's record must hold at least.
pub const LOG_RECORD_LEAST: usize = if GAP_BYTES > ROTATED_BYTES {
    GAP_BYTES
} else {
    ROTATED_BYTES
};

/// An output record's head, which its bytes follow.
pub fn log_output_head(channel: Channel, pos: LogPos, at: u64) -> [u8; LOG_OUTPUT_HEAD] {
    head(OUTPUT, channel, pos, at)
}

fn head(kind: u8, channel: Channel, pos: LogPos, at: u64) -> [u8; LOG_OUTPUT_HEAD] {
    let mut out = [0; LOG_OUTPUT_HEAD];
    let kinds = [kind, channel as u8];
    for (slot, byte) in out
        .iter_mut()
        .zip(kinds.iter().chain(&pos.to_bytes()).chain(&at.to_be_bytes()))
    {
        *slot = *byte;
    }
    out
}

impl LogRecord {
    /// Its bytes, as a lane's record carries them, appended to `out`.
    pub fn write_to(&self, out: &mut Vec<u8>) -> Result<(), managed::Error> {
        let room = |out: &mut Vec<u8>, n: usize| {
            out.try_reserve(n)
                .map_err(|_| managed::Error::Admission(managed::AdmissionError::Allocation))
        };
        match self {
            LogRecord::Output {
                channel,
                pos,
                at,
                bytes,
            } => {
                room(out, LOG_OUTPUT_HEAD.saturating_add(bytes.len()))?;
                out.extend_from_slice(&head(OUTPUT, *channel, *pos, *at));
                out.extend_from_slice(bytes);
            }
            LogRecord::Gap {
                channel,
                pos,
                at,
                lost,
                why,
            } => {
                room(out, GAP_BYTES)?;
                out.extend_from_slice(&head(GAP, *channel, *pos, *at));
                out.extend_from_slice(&lost.to_be_bytes());
                out.push(*why as u8);
            }
            LogRecord::Rotated { oldest } => {
                room(out, ROTATED_BYTES)?;
                out.push(ROTATED);
                out.extend_from_slice(&oldest.to_bytes());
            }
        }
        Ok(())
    }

    /// The record `bytes` carries, its output's bytes taken from it as they are: a protocol
    /// error for any other ([`LogRecord::check`]).
    pub fn read(mut bytes: Vec<u8>) -> Result<LogRecord, Reason> {
        let kind = *bytes.first().ok_or(Reason::Protocol)?;
        if kind == ROTATED {
            let oldest = bytes
                .get(1..)
                .and_then(|rest| <&[u8; LogPos::BYTES]>::try_from(rest).ok())
                .ok_or(Reason::Protocol)?;
            return Ok(LogRecord::Rotated {
                oldest: LogPos::from_bytes(oldest),
            });
        }
        let head = bytes.first_chunk::<LOG_OUTPUT_HEAD>().ok_or(Reason::Protocol)?;
        let (_, rest) = head.split_first_chunk::<1>().ok_or(Reason::Protocol)?;
        let ([channel], rest) = rest.split_first_chunk::<1>().ok_or(Reason::Protocol)?;
        let channel = Channel::try_from(*channel).map_err(|_| Reason::Protocol)?;
        let (pos, at) = rest
            .split_first_chunk::<{ LogPos::BYTES }>()
            .ok_or(Reason::Protocol)?;
        let pos = LogPos::from_bytes(pos);
        let at = <[u8; 8]>::try_from(at)
            .map(u64::from_be_bytes)
            .map_err(|_| Reason::Protocol)?;
        match kind {
            // Its log keeps no record of nothing.
            OUTPUT if bytes.len() > LOG_OUTPUT_HEAD => {
                bytes.drain(..LOG_OUTPUT_HEAD);
                Ok(LogRecord::Output {
                    channel,
                    pos,
                    at,
                    bytes,
                })
            }
            GAP => {
                let tail = bytes.get(LOG_OUTPUT_HEAD..).ok_or(Reason::Protocol)?;
                let (lost, why) = tail.split_first_chunk::<8>().ok_or(Reason::Protocol)?;
                let why = match why {
                    [why] => Gapped::try_from(*why).map_err(|()| Reason::Protocol)?,
                    _ => return Err(Reason::Protocol),
                };
                Ok(LogRecord::Gap {
                    channel,
                    pos,
                    at,
                    lost: u64::from_be_bytes(*lost),
                    why,
                })
            }
            _ => Err(Reason::Protocol),
        }
    }

    /// Whether `bytes` carries a record, of a known kind and its length, read without taking
    /// it: what the controller's driver holds a log lane's records to.
    pub fn check(bytes: &[u8]) -> bool {
        match bytes.first() {
            Some(&ROTATED) => bytes.len() == ROTATED_BYTES,
            Some(&OUTPUT) => bytes.len() > LOG_OUTPUT_HEAD && channel_ok(bytes),
            Some(&GAP) => {
                bytes.len() == GAP_BYTES
                    && channel_ok(bytes)
                    && bytes.last().is_some_and(|why| Gapped::try_from(*why).is_ok())
            }
            _ => false,
        }
    }

    /// Where a lane that had it goes on: past its bytes, past a gap's record, or at the oldest
    /// kept.
    pub fn after(&self) -> Option<LogPos> {
        match self {
            LogRecord::Output { pos, bytes, .. } => pos.after(u64::try_from(bytes.len()).ok()?),
            LogRecord::Gap { pos, .. } => pos.next_record(),
            LogRecord::Rotated { oldest } => Some(*oldest),
        }
    }
}

/// Whether a record's channel byte, its second, names a channel.
fn channel_ok(bytes: &[u8]) -> bool {
    bytes
        .get(1)
        .is_some_and(|channel| Channel::try_from(*channel).is_ok())
}

/// The run an exec's streams are scoped to: the run's identity and the exec's number, hashed,
/// so that no stream of one exec is another's or a member's (FIPS 180-4).
pub fn exec_run(run: RunId, exec: u32) -> RunId {
    let mut hash = Sha256::new();
    hash.update(b"shards-control/1 exec ");
    hash.update(run.0);
    hash.update(exec.to_be_bytes());
    RunId(hash.finalize().into())
}

/// What the controller steers a running exec with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steer {
    /// Its terminal's size: rows, then columns (step 6's `EXEC_RESIZE`).
    Resize(Size),
    /// A signal, by its Linux number, `1..=run::NSIG` (step 6's `EXEC_SIGNAL`).
    Signal(u32),
}

/// What an exec lane's steering frame at the front of `bytes` says, and its length; none until
/// it has come whole. Another frame, or a signal no guest has, is the controller's error.
pub fn steer(bytes: &[u8]) -> Result<Option<(Steer, usize)>, Reason> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| Reason::Protocol)? else {
        return Ok(None);
    };
    let expected = match tag {
        Tag::Resize | Tag::Signal => size_of::<u32>(),
        _ => return Err(Reason::Protocol),
    };
    if len != expected {
        return Err(Reason::Protocol);
    }
    let Some(payload) = bytes.get(FRAME_HEAD..FRAME_HEAD.saturating_add(len)) else {
        return Ok(None);
    };
    let steer = if tag == Tag::Resize {
        let (rows, cols) = frame::read_size(payload).ok_or(Reason::Protocol)?;
        Steer::Resize(Size { rows, cols })
    } else {
        let signal = u32::from_be_bytes(<[u8; 4]>::try_from(payload).map_err(|_| Reason::Protocol)?);
        if !(1..=run::NSIG).contains(&signal) {
            return Err(Reason::Protocol);
        }
        Steer::Signal(signal)
    };
    Ok(Some((steer, FRAME_HEAD.saturating_add(len))))
}

/// A steering frame: `steer`, as the controller sends it.
pub fn steering(steer: Steer) -> Result<Vec<u8>, managed::Error> {
    let mut out = Vec::new();
    match steer {
        Steer::Resize(size) => frame::put(&mut out, Tag::Resize, &frame::size(size.rows, size.cols))?,
        Steer::Signal(signal) => frame::put(&mut out, Tag::Signal, &signal.to_be_bytes())?,
    }
    Ok(out)
}

/// Why an exec did not start, typed: what its controller matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Unstarted {
    /// The run's budget has no room for another exec's streams (the host charges each).
    Capacity = 16,
    /// The member refused it, as its init refuses step 6's member exec (D243): a member that
    /// has ended or is unknown, a user or privilege the member has not, or its confinement.
    Refused = 17,
    /// It could not be started: its host could not run it.
    Failed = 18,
}

impl TryFrom<u32> for Unstarted {
    type Error = ();
    fn try_from(code: u32) -> Result<Self, ()> {
        match code {
            16 => Ok(Unstarted::Capacity),
            17 => Ok(Unstarted::Refused),
            18 => Ok(Unstarted::Failed),
            _ => Err(()),
        }
    }
}

/// Why a member's steer was not taken, typed: what its controller matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Unsteered {
    /// The member's command has ended, as kill(2) refuses a process that has (ESRCH): its end
    /// is told next.
    Ended = 19,
    /// A resize of a member with no terminal of its own (D244).
    Terminal = 20,
    /// The run's budget has no room for the steer's delivery, which its host charges while it
    /// goes: a later one may have it.
    Capacity = 21,
    /// It could not be delivered: its host or its guest could not.
    Failed = 22,
}

impl TryFrom<u32> for Unsteered {
    type Error = ();
    fn try_from(code: u32) -> Result<Self, ()> {
        match code {
            19 => Ok(Unsteered::Ended),
            20 => Ok(Unsteered::Terminal),
            21 => Ok(Unsteered::Capacity),
            22 => Ok(Unsteered::Failed),
            _ => Err(()),
        }
    }
}

/// How a member's command ended, as its lane tells it (D238): exited, with its status; or
/// killed by a signal, its Linux number (`1..=run::NSIG`), as its init's reaper saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Exited(i32),
    Signaled(u32),
}

impl End {
    /// Its status as `docker run` reports one: its exit's, or 128 plus its signal's number.
    pub fn status(self) -> i32 {
        match self {
            End::Exited(status) => status,
            End::Signaled(signal) => i32::try_from(signal).map_or(i32::MAX, |n| n.saturating_add(128)),
        }
    }
}

/// What the endpoint tells an exec's lane, or a member's, after its ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Told {
    /// It started, its streams numbered `exec` ([`exec_run`]).
    Started(u32),
    /// It did not start; a member's lane, that its host serves none.
    Unstarted(Unstarted),
    /// It ended, with this status: its command's, or 128 plus the signal that ended it. A
    /// member's, as its end is told (D231), first where it had ended before its lane asked.
    Exit(i32),
    /// A member's lane: its command was killed by this signal (`Tag::Signaled`), in place of
    /// its `Exit`.
    Signaled(u32),
    /// A member's lane: the member of this index runs.
    Running(u32),
    /// A member's lane: its last steer taken, or why not.
    Steered(Result<(), Unsteered>),
    /// A log lane: why it reads nothing.
    Unlogged(Unlogged),
}

/// A told frame of a word: its head and a u32.
pub(super) const TOLD_BYTES: usize = FRAME_HEAD + size_of::<u32>();
/// The longest told frame: a log lane's `Rotated`, its head and a place.
pub(super) const TOLD_MOST: usize = FRAME_HEAD + LogPos::BYTES;

/// `told`'s frame, in the first of its bytes this says.
pub fn telling(told: Told) -> ([u8; TOLD_MOST], usize) {
    let (tag, word) = match told {
        Told::Started(exec) => (Tag::Started, exec),
        Told::Unstarted(why) => (Tag::Refused, why as u32),
        Told::Exit(status) => (Tag::Exit, u32::from_be_bytes(status.to_be_bytes())),
        Told::Signaled(signal) => (Tag::Signaled, signal),
        Told::Running(member) => (Tag::Running, member),
        Told::Steered(answer) => (Tag::Steered, answer.err().map_or(0, |why| why as u32)),
        Told::Unlogged(Unlogged::Rotated { oldest }) => return framed(Tag::Rotated, &oldest.to_bytes()),
        Told::Unlogged(Unlogged::Unkept) => (Tag::Refused, UNKEPT),
        Told::Unlogged(Unlogged::Uncached) => (Tag::Refused, UNCACHED),
        Told::Unlogged(Unlogged::Unknown) => (Tag::Refused, UNKNOWN),
    };
    framed(tag, &word.to_be_bytes())
}

/// `payload` framed as `tag`, at most [`TOLD_MOST`] bytes whole, and its length.
fn framed(tag: Tag, payload: &[u8]) -> ([u8; TOLD_MOST], usize) {
    let mut out = [0; TOLD_MOST];
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX).to_be_bytes();
    let mut whole = 0;
    for (slot, byte) in out
        .iter_mut()
        .zip(std::iter::once(&(tag as u8)).chain(&len).chain(payload))
    {
        *slot = *byte;
        whole += 1;
    }
    (out, whole)
}

/// What the endpoint told at the front of `bytes`, and its length; none until it has come
/// whole. A lane's own refusal (`Reason`) is the lane's, read before ([`super::read_refused`]).
pub fn told(bytes: &[u8]) -> Result<Option<(Told, usize)>, Reason> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| Reason::Protocol)? else {
        return Ok(None);
    };
    let expected = match tag {
        Tag::Rotated => LogPos::BYTES,
        _ => size_of::<u32>(),
    };
    if len != expected {
        return Err(Reason::Protocol);
    }
    let whole = FRAME_HEAD.saturating_add(len);
    let Some(payload) = bytes.get(FRAME_HEAD..whole) else {
        return Ok(None);
    };
    if tag == Tag::Rotated {
        let oldest = <&[u8; LogPos::BYTES]>::try_from(payload).map_err(|_| Reason::Protocol)?;
        let oldest = LogPos::from_bytes(oldest);
        return Ok(Some((Told::Unlogged(Unlogged::Rotated { oldest }), whole)));
    }
    let word = <[u8; 4]>::try_from(payload)
        .map(u32::from_be_bytes)
        .map_err(|_| Reason::Protocol)?;
    let told = match tag {
        Tag::Started => Told::Started(word),
        Tag::Refused => match (Unstarted::try_from(word), Unlogged::of(word)) {
            (Ok(why), _) => Told::Unstarted(why),
            (Err(()), Some(why)) => Told::Unlogged(why),
            (Err(()), None) => return Err(Reason::Protocol),
        },
        Tag::Exit => Told::Exit(i32::from_be_bytes(word.to_be_bytes())),
        Tag::Signaled if (1..=run::NSIG).contains(&word) => Told::Signaled(word),
        Tag::Running => Told::Running(word),
        Tag::Steered if word == 0 => Told::Steered(Ok(())),
        Tag::Steered => Told::Steered(Err(Unsteered::try_from(word).map_err(|()| Reason::Protocol)?)),
        _ => return Err(Reason::Protocol),
    };
    Ok(Some((told, whole)))
}

/// What the host says of an exec it was handed, or of a member's lane ([`MemberStart`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Started,
    Unstarted(Unstarted),
    Exited(i32),
    /// A member's lane: its command was killed by this signal, its Linux number. An exec's
    /// lane tells it as its status, 128 plus the number.
    Signaled(u32),
    /// A member's lane: the member runs.
    Running,
    /// A member's lane: the last steer it was handed, taken or refused.
    Steered(Result<(), Unsteered>),
    /// A log lane: why it reads nothing, said in place of [`Reply::Started`].
    Unlogged(Unlogged),
}

/// Where a host says how an exec went: to the endpoint that handed it, which it wakes.
#[derive(Debug)]
pub struct Replies {
    exec: u32,
    tx: mpsc::Sender<(u32, Reply)>,
    wake: Sender<()>,
}

impl Replies {
    pub(super) fn new(exec: u32, tx: mpsc::Sender<(u32, Reply)>, wake: Sender<()>) -> Replies {
        Replies { exec, tx, wake }
    }

    /// Tells the endpoint `reply`. An endpoint gone hears nothing; a full handoff is a wake
    /// queued already, whose turn reads every reply.
    pub fn tell(&self, reply: Reply) {
        if self.tx.send((self.exec, reply)).is_ok() {
            self.took();
        }
    }

    /// Says the host took a steer: the endpoint reads the exec's lane on.
    pub fn took(&self) {
        let _ = self.wake.try_send(crate::net::Command::Handler(()));
    }

    /// Another handle to the same exec's replies, for another of its host's threads.
    pub fn try_clone(&self) -> Result<Replies, crate::Error> {
        Ok(Replies {
            exec: self.exec,
            tx: self.tx.clone(),
            wake: self.wake.try_clone()?,
        })
    }
}

/// An exec the endpoint hands its host: the ask, the exec's number, its streams' handles as
/// step 5's endpoints give them, its steering and where to say how it went. A log lane's
/// ([`Kind::Log`]) is one too: its records go into `stdout`, each a [`LogRecord`] within the
/// stream's record, it takes no steering, and its host says `Started` or why it reads none
/// ([`Reply::Unlogged`]), then `Exited(0)` once it has finished `stdout`.
#[derive(Debug)]
pub struct Start {
    pub ask: Ask,
    pub exec: u32,
    pub stdout: (Producer, managed::Control),
    /// None on a terminal, whose output is its stdout alone.
    pub stderr: Option<(Producer, managed::Control)>,
    /// None for an exec that reads no stdin.
    pub stdin: Option<(input::Relay, input::Control)>,
    /// The controller's resizes and signals, one in flight: the endpoint reads its lane on as
    /// each is taken ([`Replies::took`]). It closes once the endpoint has let the exec go (its
    /// exit told, its outputs finished; or its connection gone, which hangs it up): a host
    /// keeps each stream's control until then, as a control dropped revokes its stream.
    pub steer: mpsc::Receiver<Steer>,
    pub replies: Replies,
}

/// A member's lane the endpoint hands its host ([`Kind::Member`]): the ask, the lane's number,
/// its steering and where to say how it goes. The host says first that the member runs
/// ([`Reply::Running`]), or how it ended ([`Reply::Exited`], [`Reply::Signaled`]), or why it
/// serves the lane none ([`Reply::Unstarted`]); then answers every steer it takes
/// ([`Reply::Steered`]), the next handed only once the last is answered; and says the member's
/// end as it comes ([`Reply::Exited`], [`Reply::Signaled`]). Its steering closes as the endpoint lets the lane go: its exit told,
/// or its controller's connection gone. The member's life is no lane's: none ends it.
#[derive(Debug)]
pub struct MemberStart {
    pub ask: Ask,
    pub lane: u32,
    pub steer: mpsc::Receiver<Steer>,
    pub replies: Replies,
}

/// The admission each exec's streams get: its outputs', and its stdin's, as the run's own
/// streams get theirs (`managed::Admitted`).
#[derive(Debug, Clone, Copy)]
pub struct Admissions {
    pub output: managed::Admitted,
    pub input: managed::Admitted,
}

/// What the endpoint holds of the connection's limits (RFC 9000 §4.1, §4.6): the streams its
/// controller may have open at once, its receive window and its send window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Limits {
    pub(super) streams: u64,
    pub(super) receive: u64,
    pub(super) send: u64,
}

impl Limits {
    pub(super) fn plus(self, other: Limits) -> Option<Limits> {
        Some(Limits {
            streams: self.streams.checked_add(other.streams)?,
            receive: self.receive.checked_add(other.receive)?,
            send: self.send.checked_add(other.send)?,
        })
    }
}

/// Where an exec lane stands, or a member's (R04).
#[derive(Debug)]
pub(super) enum Stage {
    /// Its ask coming: `frame` what has, of `whole` bytes.
    Asking { frame: Vec<u8>, whole: usize },
    /// Handed to the host, which has not said yet.
    Pending,
    /// Started, or its member running: steering read, each taken (an exec's) or answered (a
    /// member's) before the next ([`Steer`]).
    Running,
    /// Its host said it ended: its exit is told once its outputs have finished and, on a
    /// member's lane, the steer its host was handed is answered.
    Exited(End),
    /// Its last frame (its exit, or why it did not start) is queued: its lane finishes as it
    /// goes, and the exec's streams with it.
    Told,
}

/// A member's lane's steering, past an exec's (R04): one steer at a time is handed to its
/// host and answered, and an answer waits for the frame before it to have gone whole.
#[derive(Debug)]
pub(super) struct Steering {
    /// The member, which its `Running` names.
    member: u32,
    /// A steer is with its host, its answer not yet come.
    awaiting: bool,
    /// Its answer, come and not yet staged.
    answer: Option<Result<(), Unsteered>>,
}

impl Steering {
    /// Whether the next steer may be read: none with its host, and no answer waiting to go.
    fn ready(&self) -> bool {
        !self.awaiting && self.answer.is_none()
    }
}

/// One exec lane, and the exec it asked for.
#[derive(Debug)]
pub(super) struct Lane {
    pub(super) conn: crate::net::ConnKey,
    pub(super) stream: hyper_quic::StreamId,
    pub(super) stage: Stage,
    /// Its number, once asked for.
    pub(super) exec: Option<u32>,
    /// What its started exec added to the connection's limits.
    pub(super) added: Limits,
    /// Its steering, to the host; dropped, the host hangs the exec up.
    pub(super) steer: Option<mpsc::SyncSender<Steer>>,
    /// A steer the host has not taken yet: the lane is read no more until it has.
    pub(super) pending: Option<Steer>,
    /// Steering frames as they come.
    pub(super) inbound: Vec<u8>,
    /// What it says, its first `out_len` bytes, as far as it has gone.
    pub(super) out: [u8; TOLD_MOST],
    pub(super) out_len: usize,
    pub(super) written: usize,
    pub(super) queued: bool,
    /// A member's lane's steering, once its ask is admitted; none on an exec's.
    pub(super) member: Option<Steering>,
    /// Whether it is a log lane, which alone is told why it reads none ([`Unlogged`]).
    pub(super) log: bool,
}

impl Lane {
    /// Queues `told`, the frame before it gone whole.
    fn say(&mut self, told: Told) {
        (self.out, self.out_len) = telling(told);
        self.written = 0;
        self.queued = true;
    }
}

/// The endpoint's execs and members' lanes: what each exec's streams are admitted, where it
/// hands its host each exec and each member's lane, and what the host says.
#[derive(Debug)]
pub(super) struct Execs {
    /// The run an ask names and its generation: its member streams', or, at an endpoint of
    /// none (an orchestrator's run host's, D252), the zero run and generation 0, as a socket
    /// lane names them there (unix.rs). Fixed at its bind: an exec's own streams, which join
    /// its outputs later, are never its scope.
    pub(super) run: (RunId, u64),
    pub(super) admissions: Option<Admissions>,
    pub(super) starts: Option<mpsc::Sender<Start>>,
    /// Each member's lane, where its host runs execs: the same host serves both.
    pub(super) members: Option<mpsc::Sender<MemberStart>>,
    pub(super) replies_tx: mpsc::Sender<(u32, Reply)>,
    pub(super) replies: mpsc::Receiver<(u32, Reply)>,
    /// The endpoint's own sender, which each exec's handles wake it with.
    pub(super) wake: Option<Sender<()>>,
    pub(super) next: u32,
    pub(super) lanes: Vec<Lane>,
    /// The connection's limits without an exec running: its streams' and one ask's.
    pub(super) base: Limits,
}

/// What the endpoint hands its host: each exec, and each member's lane.
pub(super) type Handed = (Option<mpsc::Receiver<Start>>, Option<mpsc::Receiver<MemberStart>>);

impl Execs {
    pub(super) fn new(admissions: Option<Admissions>, base: Limits, run: (RunId, u64)) -> (Execs, Handed) {
        let (replies_tx, replies) = mpsc::channel();
        let ((starts, handed), (members, lanes)) = match admissions {
            Some(_) => {
                let (tx, rx) = mpsc::channel();
                let (members, lanes) = mpsc::channel();
                ((Some(tx), Some(rx)), (Some(members), Some(lanes)))
            }
            None => ((None, None), (None, None)),
        };
        (
            Execs {
                run,
                admissions,
                starts,
                members,
                replies_tx,
                replies,
                wake: None,
                next: 0,
                lanes: Vec::new(),
                base,
            },
            (handed, lanes),
        )
    }

    /// The connection's limits with every started exec of `conn` in.
    pub(super) fn limits(&self, conn: crate::net::ConnKey) -> Option<Limits> {
        self.lanes
            .iter()
            .filter(|l| l.conn == conn)
            .try_fold(self.base, |sum, l| sum.plus(l.added))
    }
}

/// What a started exec adds to its connection's limits: its lane stays open and another ask
/// may come, and each of its stdio lanes; what the endpoint reads of them before they are read
/// (an output's subscription and acknowledgment, a stdin's record and end, a steer) and what it
/// sends on them unacknowledged (each output's admitted window, a stdin's taken, a refusal
/// each, the lane's told frames), as the endpoint sums its own streams'
/// (`super::Endpoint::bind`).
pub(super) fn added(admissions: Admissions, stderr: bool, stdin: bool) -> Option<Limits> {
    let outputs = 1u64.checked_add(u64::from(stderr))?;
    let output = Limits {
        streams: 1,
        receive: u64::try_from(managed::CONTROL_BYTES).ok()?,
        send: admissions
            .output
            .transport_bytes
            .checked_add(u64::try_from(super::REFUSED_BYTES).ok()?)?,
    };
    let mut sum = lane_added()?;
    for _ in 0..outputs {
        sum = sum.plus(output)?;
    }
    if stdin {
        sum = sum.plus(Limits {
            streams: 1,
            receive: u64::try_from(input::receive_bytes(admissions.input.record_bytes).ok()?).ok()?,
            send: admissions
                .input
                .transport_bytes
                .checked_add(u64::try_from(super::REFUSED_BYTES).ok()?)?,
        })?;
    }
    Some(sum)
}

/// What a lane kept open adds, an exec's or a member's: its stream, and the next ask's; the
/// steer it reads at once; the frame it tells at once, one in flight, the next as it has room.
fn lane_added() -> Option<Limits> {
    Some(Limits {
        streams: 1,
        receive: u64::try_from(STEER_BYTES).ok()?,
        send: u64::try_from(TOLD_BYTES).ok()?,
    })
}

/// What a started exec's lanes may hold at its endpoint while it runs, received and sent:
/// the windows the endpoint raises its connection's by ([`added`]), for its host to charge
/// the run's budget with (D238). None where they overflow.
pub fn lane_bytes(admissions: Admissions, stderr: bool, stdin: bool) -> Option<u64> {
    let added = added(admissions, stderr, stdin)?;
    added.receive.checked_add(added.send)
}

/// What a member's lane may hold at its endpoint while it is open, received and sent: its own
/// windows alone ([`lane_added`]), as it makes no stream, for its host to charge the run's
/// budget with. None where they overflow.
pub fn member_lane_bytes() -> Option<u64> {
    let added = lane_added()?;
    added.receive.checked_add(added.send)
}

/// A steering frame's length: its head and its four bytes.
const STEER_BYTES: usize = FRAME_HEAD + size_of::<u32>();

impl super::Lanes {
    /// A lane whose first frame is an ask (`Tag::MemberExec`, `Tag::Mcp` for an MCP
    /// connection, or `Tag::Member` for a member's own lane) of `len` bytes, `bytes` what came
    /// of it: refused alone where the endpoint serves no exec or the ask is longer than the run
    /// protocol carries; read on as it comes otherwise.
    pub(super) fn begin_ask(
        &mut self,
        io: &mut crate::net::Io<'_>,
        conn: crate::net::ConnKey,
        stream: hyper_quic::StreamId,
        bytes: &[u8],
        len: usize,
        ended: bool,
    ) {
        if self.execs.admissions.is_none() || len > ASK_MOST {
            let why = if len > ASK_MOST {
                Reason::Protocol
            } else {
                Reason::Scope
            };
            self.refuse_lane(io, conn, stream, why);
            return;
        }
        let mut frame = Vec::new();
        if frame.try_reserve(bytes.len()).is_err() || self.execs.lanes.try_reserve(1).is_err() {
            self.refuse_lane(io, conn, stream, Reason::Closed);
            return;
        }
        frame.extend_from_slice(bytes);
        self.execs.lanes.push(Lane {
            conn,
            stream,
            stage: Stage::Asking {
                frame,
                whole: FRAME_HEAD.saturating_add(len),
            },
            exec: None,
            added: Limits::default(),
            steer: None,
            pending: None,
            inbound: Vec::new(),
            out: [0; TOLD_MOST],
            out_len: 0,
            written: 0,
            queued: false,
            member: None,
            log: false,
        });
        if !ended {
            self.read_exec(io, conn, stream);
            return;
        }
        // Its controller ended its side with the ask: whole, the exec runs unsteered.
        let at = self.execs.lanes.len().saturating_sub(1);
        let whole = FRAME_HEAD.saturating_add(len);
        match Some(bytes).filter(|b| b.len() == whole).map(Ask::decode) {
            Some(Ok(ask)) => self.admit_exec(io, at, ask),
            Some(Err(why)) => {
                self.execs.lanes.swap_remove(at);
                self.refuse_lane(io, conn, stream, why);
            }
            None => {
                self.execs.lanes.swap_remove(at);
                super::end(io, conn, stream);
            }
        }
    }

    /// Reads the exec lane `(conn, stream)` as far as its stage reads: its ask, then its
    /// steering, each steer taken by the host before the next is read. Whether it is one.
    pub(super) fn read_exec(
        &mut self,
        io: &mut crate::net::Io<'_>,
        conn: crate::net::ConnKey,
        stream: hyper_quic::StreamId,
    ) -> bool {
        let Some(at) = self
            .execs
            .lanes
            .iter()
            .position(|l| l.conn == conn && l.stream == stream)
        else {
            return false;
        };
        loop {
            let Some(lane) = self.execs.lanes.get_mut(at) else {
                return true;
            };
            let room = match &lane.stage {
                Stage::Asking { frame, whole } => whole.saturating_sub(frame.len()),
                Stage::Running
                    if lane.pending.is_none() && lane.member.as_ref().is_none_or(Steering::ready) =>
                {
                    STEER_BYTES.saturating_sub(lane.inbound.len())
                }
                // Held: flow control holds its controller back meanwhile.
                _ => return true,
            };
            // An ask the lane's first read took whole (an MCP connection's, its scope alone, is
            // shorter than `LANE_MOST`) is read no further: a read of no room is blocked.
            let whole = room == 0 && matches!(lane.stage, Stage::Asking { .. });
            let read = if whole {
                Ok(crate::net::Read::Bytes(Vec::new()))
            } else {
                io.read_within(conn, stream, room)
            };
            let (bytes, ended) = match read {
                Ok(crate::net::Read::Bytes(bytes)) => (bytes, false),
                Ok(crate::net::Read::End(bytes)) => (bytes, true),
                Ok(crate::net::Read::Blocked) => return true,
                Ok(crate::net::Read::Reset) | Err(_) => {
                    // Its controller left it: an exec it started is hung up.
                    lane.steer = None;
                    if matches!(lane.stage, Stage::Asking { .. }) {
                        self.execs.lanes.swap_remove(at);
                    }
                    super::end(io, conn, stream);
                    return true;
                }
            };
            match &mut lane.stage {
                Stage::Asking { frame, whole } => {
                    if frame.try_reserve(bytes.len()).is_err() {
                        self.execs.lanes.swap_remove(at);
                        self.refuse_lane(io, conn, stream, Reason::Closed);
                        return true;
                    }
                    frame.extend_from_slice(&bytes);
                    if frame.len() < *whole {
                        if ended {
                            self.execs.lanes.swap_remove(at);
                            super::end(io, conn, stream);
                            return true;
                        }
                        continue;
                    }
                    let asked = Ask::decode(frame.as_slice());
                    match asked {
                        Ok(ask) => self.admit_exec(io, at, ask),
                        Err(why) => {
                            self.execs.lanes.swap_remove(at);
                            self.refuse_lane(io, conn, stream, why);
                        }
                    }
                    return true;
                }
                Stage::Running => {
                    lane.inbound.extend_from_slice(&bytes);
                    match steer(&lane.inbound) {
                        Ok(Some((taken, len))) => {
                            lane.inbound.drain(..len.min(lane.inbound.len()));
                            lane.pending = Some(taken);
                            Self::hand_steer(lane);
                        }
                        Ok(None) if !ended => {}
                        Ok(None) => {
                            // Its controller steers it no more; it runs on.
                            io.stop(conn, stream);
                            return true;
                        }
                        Err(why) => {
                            self.refuse_connection(io, conn, why);
                            return true;
                        }
                    }
                }
                _ => return true,
            }
        }
    }

    /// Hands a lane's pending steer to its host, where it has room for it: a member's then
    /// awaits its answer.
    fn hand_steer(lane: &mut Lane) {
        let Some(steer) = lane.pending else {
            return;
        };
        let Some(to) = &lane.steer else {
            // A member's steer that no host takes is answered as one it could not deliver.
            if let Some(member) = lane.member.as_mut() {
                lane.pending = None;
                member.answer = Some(Err(Unsteered::Failed));
            }
            return;
        };
        match to.try_send(steer) {
            Ok(()) => {
                lane.pending = None;
                if let Some(member) = lane.member.as_mut() {
                    member.awaiting = true;
                }
            }
            Err(mpsc::TrySendError::Full(_)) => {}
            // Its host has let it go: what still comes is let go too, and a member's steer is
            // answered as one its host could not deliver.
            Err(mpsc::TrySendError::Disconnected(_)) => {
                lane.pending = None;
                lane.steer = None;
                if let Some(member) = lane.member.as_mut() {
                    member.answer = Some(Err(Unsteered::Failed));
                }
            }
        }
    }

    /// An ask read whole: refused alone for another run, generation or member than the
    /// endpoint serves, or where it numbers no more lanes; else an exec's streams are made, as
    /// step 5's endpoints make theirs, and the exec handed to the host, or a member's lane
    /// handed to it as it is.
    fn admit_exec(&mut self, io: &mut crate::net::Io<'_>, at: usize, ask: Ask) {
        let Some(lane) = self.execs.lanes.get(at) else {
            return;
        };
        let (conn, stream) = (lane.conn, lane.stream);
        let member = (ask.kind == Kind::Member).then_some(ask.member);
        let log = matches!(ask.kind, Kind::Log(_));
        let handed = match member {
            Some(_) => self.hand_member(&ask),
            None => self.hand_exec(io, &ask),
        };
        match handed {
            Ok((number, steer)) => {
                if let Some(lane) = self.execs.lanes.get_mut(at) {
                    lane.stage = Stage::Pending;
                    lane.exec = Some(number);
                    lane.steer = Some(steer);
                    lane.log = log;
                    lane.member = member.map(|member| Steering {
                        member,
                        awaiting: false,
                        answer: None,
                    });
                }
            }
            Err(why) => {
                self.execs.lanes.swap_remove(at);
                self.refuse_lane(io, conn, stream, why);
            }
        }
    }

    /// Whether the endpoint admits `ask`: its run's, its generation, a member it serves, and
    /// a host that runs what is asked. An endpoint of member streams serves their members
    /// alone; one of none leaves the member to its host, which knows its run's members. A
    /// log's source is its host's to know: the run keeps every member's output (D240) and its
    /// command's, whichever streams the endpoint serves.
    fn scoped(&self, ask: &Ask) -> Result<(), Reason> {
        let (run, generation) = self.execs.run;
        if ask.run != run {
            return Err(Reason::Scope);
        }
        if ask.generation != generation {
            return Err(Reason::Stale);
        }
        let mut members = self.outputs.iter().filter(|o| o.scope().run == run).peekable();
        let served = members.peek().is_none() || members.any(|o| o.scope().member == ask.member);
        if !served && !matches!(ask.kind, Kind::Log(_)) {
            return Err(Reason::Scope);
        }
        self.execs.admissions.map(drop).ok_or(Reason::Scope)
    }

    /// The exec `ask` names, made and handed to its host: its number and its steering.
    fn hand_exec(
        &mut self,
        io: &mut crate::net::Io<'_>,
        ask: &Ask,
    ) -> Result<(u32, mpsc::SyncSender<Steer>), Reason> {
        let (exec, start, steer) = self.make_exec(ask)?;
        let sent = self
            .execs
            .starts
            .as_ref()
            .is_some_and(|starts| starts.send(start).is_ok());
        if !sent {
            self.drop_exec(io, exec);
            return Err(Reason::Closed);
        }
        Ok((exec, steer))
    }

    /// The member's lane `ask` names, handed to its host: its number and its steering, one
    /// steer at a time.
    fn hand_member(&mut self, ask: &Ask) -> Result<(u32, mpsc::SyncSender<Steer>), Reason> {
        self.scoped(ask)?;
        let wake = self.execs.wake.as_ref().ok_or(Reason::Closed)?;
        let lane = self.execs.next;
        let next = lane.checked_add(1).ok_or(Reason::Closed)?;
        let replies = Replies::new(
            lane,
            self.execs.replies_tx.clone(),
            wake.try_clone().map_err(|_| Reason::Closed)?,
        );
        let (steer, steering) = mpsc::sync_channel(1);
        let start = MemberStart {
            ask: ask.clone(),
            lane,
            steer: steering,
            replies,
        };
        let sent = self
            .execs
            .members
            .as_ref()
            .is_some_and(|members| members.send(start).is_ok());
        if !sent {
            return Err(Reason::Closed);
        }
        self.execs.next = next;
        Ok((lane, steer))
    }

    /// The exec `ask` names, its streams made and served, and what its host is handed.
    fn make_exec(&mut self, ask: &Ask) -> Result<(u32, Start, mpsc::SyncSender<Steer>), Reason> {
        self.scoped(ask)?;
        let admissions = self.execs.admissions.ok_or(Reason::Scope)?;
        let wake = self.execs.wake.as_ref().ok_or(Reason::Closed)?;
        let exec = self.execs.next;
        let next = exec.checked_add(1).ok_or(Reason::Closed)?;
        let run = exec_run(ask.run, exec);
        let scope = |channel| managed::Scope {
            run,
            generation: ask.generation,
            member: ask.member,
            channel,
        };
        let closed = |_| Reason::Closed;
        let output = |channel| -> Result<(managed::Service, (Producer, managed::Control)), Reason> {
            let service =
                managed::Service::new(scope(channel), self.controller, admissions.output.record_bytes)
                    .map_err(closed)?;
            let handles = service
                .handles(wake.try_clone().map_err(|_| Reason::Closed)?, managed::Stop::Lane)
                .map_err(closed)?;
            Ok((service, handles))
        };
        let (stdout, stdout_handles) = output(Channel::Stdout)?;
        // An exec's stderr is its own unless it runs on a terminal; an MCP connection's bytes
        // are one stream each way; a member's lane makes none (`hand_member`).
        let stderr = match &ask.kind {
            Kind::Exec(spec) if spec.tty.is_none() => Some(output(Channel::Stderr)?),
            Kind::Exec(_) | Kind::Mcp | Kind::Member | Kind::Log(_) => None,
        };
        let reads = match &ask.kind {
            Kind::Exec(spec) => spec.stdin,
            Kind::Mcp => true,
            Kind::Member | Kind::Log(_) => false,
        };
        let stdin = match reads {
            true => {
                let service = input::Service::new(
                    input::Scope {
                        run,
                        generation: ask.generation,
                        member: ask.member,
                    },
                    self.controller,
                    admissions.input.record_bytes,
                )
                .map_err(closed)?;
                let handles = service
                    .handles(wake.try_clone().map_err(|_| Reason::Closed)?, managed::Stop::Lane)
                    .map_err(closed)?;
                Some((service, handles))
            }
            false => None,
        };
        let replies = Replies::new(
            exec,
            self.execs.replies_tx.clone(),
            wake.try_clone().map_err(|_| Reason::Closed)?,
        );
        self.outputs.try_reserve(2).map_err(|_| Reason::Closed)?;
        self.inputs.try_reserve(1).map_err(|_| Reason::Closed)?;
        let (steer_tx, steer_rx) = mpsc::sync_channel(1);
        let (stderr_service, stderr_handles) = match stderr {
            Some((service, handles)) => (Some(service), Some(handles)),
            None => (None, None),
        };
        let (stdin_service, stdin_handles) = match stdin {
            Some((service, handles)) => (Some(service), Some(handles)),
            None => (None, None),
        };
        self.outputs.push(stdout);
        if let Some(service) = stderr_service {
            self.outputs.push(service);
        }
        if let Some(service) = stdin_service {
            self.inputs.push(service);
        }
        self.execs.next = next;
        Ok((
            exec,
            Start {
                ask: ask.clone(),
                exec,
                stdout: stdout_handles,
                stderr: stderr_handles,
                stdin: stdin_handles,
                steer: steer_rx,
                replies,
            },
            steer_tx,
        ))
    }

    /// Exec `exec`'s streams let go, and its handles told it is over: a lane on an output that
    /// has finished ends as the output's own would (its FIN), whichever thread finished it last;
    /// any other lane on one is abandoned.
    fn drop_exec(&mut self, io: &mut crate::net::Io<'_>, exec: u32) {
        let run = exec_run(self.execs.run.0, exec);
        for output in self.outputs.iter_mut().filter(|o| o.scope().run == run) {
            if let (Some((conn, stream)), true) = (output.subscriber(), output.finished()) {
                output.flush(io);
                output.disconnect();
                io.stop(conn, stream);
            }
        }
        for input in self.inputs.iter_mut().filter(|i| i.scope().run == run) {
            if let (Some((conn, stream)), true) = (input.subscriber(), input.said_its_end()) {
                input.disconnect();
                io.stop(conn, stream);
            }
        }
        for (conn, stream) in self.let_exec_go(exec) {
            super::end(io, conn, stream);
        }
    }

    /// Exec `exec`'s streams let go, its handles told it is over: the lanes on them, to end.
    fn let_exec_go(&mut self, exec: u32) -> Vec<(crate::net::ConnKey, hyper_quic::StreamId)> {
        let mut lanes = Vec::new();
        let run = exec_run(self.execs.run.0, exec);
        for output in self.outputs.iter_mut().filter(|o| o.scope().run == run) {
            if let Some(lane) = output.subscriber() {
                lanes.push(lane);
            }
            output.close();
        }
        for input in self.inputs.iter_mut().filter(|i| i.scope().run == run) {
            if let Some(lane) = input.subscriber() {
                lanes.push(lane);
            }
            input.close();
        }
        self.outputs.retain(|o| o.scope().run != run);
        self.inputs.retain(|i| i.scope().run != run);
        lanes
    }

    /// Whether exec `exec`'s outputs have all ended, every record consumed and their end told
    /// their controller, or were let go. One that finished before its controller's lane came (a
    /// log lane with nothing to read) is not done until that lane has been told its end: let go
    /// sooner, the lane would be refused `Scope` for an output its endpoint served.
    fn outputs_done(&self, exec: u32) -> bool {
        let run = exec_run(self.execs.run.0, exec);
        self.outputs
            .iter()
            .filter(|o| o.scope().run == run)
            .all(|o| o.ended() || o.aborted())
    }

    /// A turn for the execs and members' lanes: what their host said; each steer it had no
    /// room for handed again; each lane's frames written in order, the next staged only once
    /// the one before has gone whole ([`Lanes::stage`]); each lane told whole let go, the
    /// connection's limits with it; and each member's lane whose answer went read on for its
    /// next steer.
    pub(super) fn exec_turn(&mut self, io: &mut crate::net::Io<'_>) {
        while let Ok((exec, reply)) = self.execs.replies.try_recv() {
            let Some(at) = self.execs.lanes.iter().position(|l| l.exec == Some(exec)) else {
                continue;
            };
            self.reply(io, at, exec, reply);
        }
        let mut answered = Vec::new();
        let mut at = 0;
        while at < self.execs.lanes.len() {
            let exec = self.execs.lanes.get(at).and_then(|l| l.exec);
            let done = exec.is_some_and(|e| self.outputs_done(e));
            let Some(lane) = self.execs.lanes.get_mut(at) else {
                break;
            };
            Self::hand_steer(lane);
            // Each frame gone whole, the next due staged: a member's answer, then its exit,
            // which nothing else would wake the endpoint for once its answer has gone.
            let mut written = Self::write_told(io, lane);
            let mut answer = false;
            while written {
                answer |= Self::stage(lane, done);
                if !lane.queued {
                    break;
                }
                written = Self::write_told(io, lane);
            }
            if answer && answered.try_reserve(1).is_ok() {
                answered.push((lane.conn, lane.stream));
            }
            if written && matches!(lane.stage, Stage::Told) {
                let (conn, added) = (lane.conn, lane.added);
                let _ = io.finish(conn, lane.stream);
                io.stop(conn, lane.stream);
                let exec = lane.exec;
                self.execs.lanes.swap_remove(at);
                if let Some(exec) = exec {
                    self.drop_exec(io, exec);
                }
                if added != Limits::default() {
                    self.set_limits(io, conn);
                }
                continue;
            }
            at = at.saturating_add(1);
        }
        for (conn, stream) in answered {
            self.read_exec(io, conn, stream);
        }
    }

    /// Stages a lane's next frame, the one before it gone whole: a member's answer to its last
    /// steer; else its exit, once its outputs have finished and no steer a member's host was
    /// handed awaits its answer. Whether it staged an answer.
    fn stage(lane: &mut Lane, done: bool) -> bool {
        if let Some(answer) = lane.member.as_mut().and_then(|m| m.answer.take()) {
            lane.say(Told::Steered(answer));
            return true;
        }
        let awaiting = lane.member.as_ref().is_some_and(|m| m.awaiting);
        let exited = match lane.stage {
            Stage::Exited(end) => Some(end),
            _ => None,
        };
        if let (Some(end), true, false) = (exited, done, awaiting) {
            lane.say(match end {
                End::Exited(status) => Told::Exit(status),
                End::Signaled(signal) => Told::Signaled(signal),
            });
            lane.stage = Stage::Told;
        }
        false
    }

    /// Writes what is left of a lane's frame; whether nothing is left.
    fn write_told(io: &mut crate::net::Io<'_>, lane: &mut Lane) -> bool {
        if !lane.queued {
            return true;
        }
        while let Some(rest) = lane.out.get(lane.written..lane.out_len).filter(|r| !r.is_empty()) {
            match io.write(lane.conn, lane.stream, rest) {
                Ok(0) => return false,
                Ok(n) => lane.written = lane.written.saturating_add(n),
                Err(_) => break,
            }
        }
        lane.queued = false;
        true
    }

    /// Acts on what the host said of exec `exec`, or of a member's lane of that number, on lane
    /// `at`.
    fn reply(&mut self, io: &mut crate::net::Io<'_>, at: usize, exec: u32, reply: Reply) {
        let admissions = self.execs.admissions;
        let Some(lane) = self.execs.lanes.get_mut(at) else {
            return;
        };
        let member = lane.member.as_ref().map(|m| m.member);
        match (reply, &lane.stage, member) {
            (Reply::Running, Stage::Pending, Some(member)) => {
                lane.added = lane_added().unwrap_or_default();
                lane.stage = Stage::Running;
                lane.say(Told::Running(member));
                self.opened(io, at);
            }
            (Reply::Steered(answer), Stage::Running | Stage::Exited(_), Some(_)) => {
                if let Some(steering) = lane.member.as_mut().filter(|m| m.awaiting) {
                    steering.awaiting = false;
                    steering.answer = Some(answer);
                }
            }
            (Reply::Started, Stage::Pending, None) => {
                let run = exec_run(self.execs.run.0, exec);
                let stderr = self.outputs.iter().filter(|o| o.scope().run == run).count() > 1;
                let stdin = self.inputs.iter().any(|i| i.scope().run == run);
                let Some(lane) = self.execs.lanes.get_mut(at) else {
                    return;
                };
                lane.added = admissions
                    .and_then(|a| added(a, stderr, stdin))
                    .unwrap_or_default();
                lane.stage = Stage::Running;
                lane.say(Told::Started(exec));
                self.opened(io, at);
            }
            (Reply::Unstarted(why), Stage::Pending, _) => {
                lane.say(Told::Unstarted(why));
                lane.stage = Stage::Told;
                lane.steer = None;
            }
            // A log lane's host reads none: said in place of its start, as an exec's that did
            // not start is.
            (Reply::Unlogged(why), Stage::Pending, None) => {
                // A host's word no exec lane has is its failure, not its controller's to read.
                lane.say(match lane.log {
                    true => Told::Unlogged(why),
                    false => Told::Unstarted(Unstarted::Failed),
                });
                lane.stage = Stage::Told;
                lane.steer = None;
            }
            (Reply::Exited(status), Stage::Running | Stage::Pending, _) => {
                // Its steering stays open until it is let go (`Start::steer`, `MemberStart`).
                lane.stage = Stage::Exited(End::Exited(status));
            }
            // A member's signal typed; an exec's lane has only a status to tell.
            (Reply::Signaled(signal), Stage::Running | Stage::Pending, member) => {
                lane.stage = Stage::Exited(match member {
                    Some(_) => End::Signaled(signal),
                    None => End::Exited(End::Signaled(signal).status()),
                });
            }
            _ => {}
        }
    }

    /// Lane `at` runs, its exec started or its member running: room for the lanes it opens,
    /// kept as its connection's own; the connection's limits raised by its own; and its
    /// steering read on.
    fn opened(&mut self, io: &mut crate::net::Io<'_>, at: usize) {
        let Some(lane) = self.execs.lanes.get(at) else {
            return;
        };
        let (conn, stream) = (lane.conn, lane.stream);
        let streams = usize::try_from(lane.added.streams)
            .ok()
            .and_then(|n| n.checked_mul(super::CONNECTIONS))
            .unwrap_or(0);
        let _ = self.opening.try_reserve(streams);
        let _ = self.answering.try_reserve(streams);
        self.set_limits(io, conn);
        self.read_exec(io, conn, stream);
    }

    /// Sets `conn`'s limits to its streams' and its started execs'.
    fn set_limits(&mut self, io: &mut crate::net::Io<'_>, conn: crate::net::ConnKey) {
        if let Some(limits) = self.execs.limits(conn) {
            let _ = io.limit(conn, limits.streams, limits.receive, limits.send);
        }
    }

    /// `conn` is gone: each exec it asked for is hung up (its steering dropped) and let go,
    /// as a session's command ends with its connection (RFC 4254 §6.10's channel close).
    pub(super) fn forget_execs(&mut self, conn: crate::net::ConnKey) {
        let mut at = 0;
        while at < self.execs.lanes.len() {
            if self.execs.lanes.get(at).is_some_and(|l| l.conn == conn) {
                let exec = self.execs.lanes.swap_remove(at).exec;
                if let Some(exec) = exec {
                    // Its lanes were the connection's, gone with it.
                    let _ = self.let_exec_go(exec);
                }
                continue;
            }
            at = at.saturating_add(1);
        }
    }

    /// The controller reads no more of exec lane `(conn, stream)`: its exec is hung up and let
    /// go, its lanes ended. Whether it was one.
    pub(super) fn stop_exec(
        &mut self,
        io: &mut crate::net::Io<'_>,
        conn: crate::net::ConnKey,
        stream: hyper_quic::StreamId,
    ) -> bool {
        let Some(at) = self
            .execs
            .lanes
            .iter()
            .position(|l| l.conn == conn && l.stream == stream)
        else {
            return false;
        };
        let lane = self.execs.lanes.swap_remove(at);
        if let Some(exec) = lane.exec {
            self.drop_exec(io, exec);
        }
        if lane.added != Limits::default() {
            self.set_limits(io, conn);
        }
        io.stop(conn, stream);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ask's frame reads back as it was sent: an exec's with step 6's spec byte for byte, an
    /// MCP connection's and a member's own lane's with their scope alone. One cut short, an
    /// exec's of a spec the run protocol would not take, an MCP or member ask that carries more
    /// than its scope, or a frame of another kind, is the controller's error.
    #[test]
    fn an_ask_reads_back_as_it_was_sent() {
        let ask = Ask {
            run: RunId([4; 32]),
            generation: 9,
            member: 2,
            kind: Kind::Exec(Box::new(Spec {
                argv: vec![b"sh".to_vec(), b"-c".to_vec(), b"printf \xff".to_vec()],
                env: vec![b"A=b".to_vec()],
                cwd: b"/work".to_vec(),
                tty: Some(Size { rows: 24, cols: 80 }),
                stdin: true,
                ..Spec::default()
            })),
        };
        let frame = ask.frame().unwrap();
        let (tag, len) = frame::peek(&frame).unwrap().unwrap();
        assert_eq!((tag, len), (Tag::MemberExec, frame.len() - FRAME_HEAD));
        assert_eq!(Ask::decode(&frame), Ok(ask.clone()));
        assert_eq!(Ask::decode(&frame[..FRAME_HEAD + SCOPE]), Err(Reason::Protocol));
        assert_eq!(Ask::decode(&frame[..FRAME_HEAD + 10]), Err(Reason::Protocol));
        let reframed = |tag, payload: &[u8]| {
            let mut out = Vec::new();
            frame::put(&mut out, tag, payload).unwrap();
            out
        };
        let scope = &frame[FRAME_HEAD..FRAME_HEAD + SCOPE];
        assert_eq!(
            Ask::decode(&reframed(Tag::MemberExec, scope)),
            Err(Reason::Protocol)
        );

        let mut longer = scope.to_vec();
        longer.push(0);
        for (kind, tag) in [(Kind::Mcp, Tag::Mcp), (Kind::Member, Tag::Member)] {
            let alone = Ask { kind, ..ask.clone() };
            let frame = alone.frame().unwrap();
            assert_eq!(frame::peek(&frame).unwrap(), Some((tag, SCOPE)));
            assert_eq!(&frame[FRAME_HEAD..], scope);
            assert_eq!(Ask::decode(&frame), Ok(alone));
            assert_eq!(Ask::decode(&reframed(tag, &longer)), Err(Reason::Protocol));
            assert_eq!(
                Ask::decode(&reframed(tag, &scope[..SCOPE - 1])),
                Err(Reason::Protocol)
            );
        }
        assert_eq!(Ask::decode(&reframed(Tag::Control, scope)), Err(Reason::Protocol));
    }

    /// Steering reads back; a signal no guest has, or another frame, is the controller's error;
    /// none until it has come whole.
    #[test]
    fn steering_reads_back_and_a_signal_no_guest_has_is_refused() {
        for sent in [
            Steer::Resize(Size { rows: 50, cols: 132 }),
            Steer::Signal(1),
            Steer::Signal(run::NSIG),
        ] {
            let frame = steering(sent).unwrap();
            assert_eq!(steer(&frame), Ok(Some((sent, STEER_BYTES))));
            assert_eq!(steer(&frame[..STEER_BYTES - 1]), Ok(None));
        }
        for signal in [0, run::NSIG + 1] {
            let mut frame = Vec::new();
            frame::put(&mut frame, Tag::Signal, &signal.to_be_bytes()).unwrap();
            assert_eq!(steer(&frame), Err(Reason::Protocol));
        }
        let mut stdin = Vec::new();
        frame::put(&mut stdin, Tag::Stdin, b"x").unwrap();
        assert_eq!(steer(&stdin), Err(Reason::Protocol));
    }

    /// What an exec lane is told reads back, its exit's sign kept, and what a member's is: that
    /// it runs, naming it, and each steer's answer, taken or refused by its code; and why a log
    /// lane reads none, `Rotated` naming its place. A refusal of a lane's own reason is no
    /// exec's word, an answer of no code is none, and a `Rotated` of no place is none.
    #[test]
    fn what_an_exec_lane_is_told_reads_back() {
        let oldest = LogPos {
            seq: 7,
            no: u64::MAX,
            byte: 1,
        };
        for said in [
            Told::Started(7),
            Told::Unstarted(Unstarted::Capacity),
            Told::Unstarted(Unstarted::Refused),
            Told::Unstarted(Unstarted::Failed),
            Told::Exit(0),
            Told::Exit(137),
            Told::Exit(-1),
            Told::Signaled(1),
            Told::Signaled(15),
            Told::Signaled(run::NSIG),
            Told::Running(3),
            Told::Steered(Ok(())),
            Told::Steered(Err(Unsteered::Ended)),
            Told::Steered(Err(Unsteered::Terminal)),
            Told::Steered(Err(Unsteered::Capacity)),
            Told::Steered(Err(Unsteered::Failed)),
            Told::Unlogged(Unlogged::Rotated { oldest }),
            Told::Unlogged(Unlogged::Unkept),
            Told::Unlogged(Unlogged::Uncached),
            Told::Unlogged(Unlogged::Unknown),
        ] {
            let (frame, len) = telling(said);
            let rotated = matches!(said, Told::Unlogged(Unlogged::Rotated { .. }));
            assert_eq!(len, if rotated { TOLD_MOST } else { TOLD_BYTES });
            assert_eq!(told(&frame[..len]), Ok(Some((said, len))));
            assert_eq!(told(&frame[..len - 1]), Ok(None));
        }
        assert_eq!(told(&super::super::refused(Reason::Scope)), Err(Reason::Protocol));
        for code in [1, 18, 23, u32::MAX] {
            let mut frame = Vec::new();
            frame::put(&mut frame, Tag::Steered, &code.to_be_bytes()).unwrap();
            assert_eq!(told(&frame), Err(Reason::Protocol), "{code}");
        }
        for code in [26, u32::MAX] {
            let mut frame = Vec::new();
            frame::put(&mut frame, Tag::Refused, &code.to_be_bytes()).unwrap();
            assert_eq!(told(&frame), Err(Reason::Protocol), "{code}");
        }
        // A signal no Linux has is no end.
        for signal in [0, run::NSIG + 1, u32::MAX] {
            let mut frame = Vec::new();
            frame::put(&mut frame, Tag::Signaled, &signal.to_be_bytes()).unwrap();
            assert_eq!(told(&frame), Err(Reason::Protocol), "{signal}");
        }
        let mut short = Vec::new();
        frame::put(&mut short, Tag::Rotated, &7u32.to_be_bytes()).unwrap();
        assert_eq!(told(&short), Err(Reason::Protocol));
    }

    /// A member's steer's refusals are their own codes, apart from an exec's (16 to 18) and a
    /// lane's (1 to 5), and each reads back as itself.
    #[test]
    fn a_steers_refusals_are_codes_of_their_own() {
        for why in [
            Unsteered::Ended,
            Unsteered::Terminal,
            Unsteered::Capacity,
            Unsteered::Failed,
        ] {
            let code = why as u32;
            assert_eq!(Unsteered::try_from(code), Ok(why));
            assert!(Unstarted::try_from(code).is_err() && Reason::try_from(code).is_err());
            assert_eq!(Unlogged::of(code), None);
        }
        // A log lane's refusals too: apart from every other, each read back as itself.
        for (code, why) in [
            (UNKEPT, Unlogged::Unkept),
            (UNCACHED, Unlogged::Uncached),
            (UNKNOWN, Unlogged::Unknown),
        ] {
            assert_eq!(Unlogged::of(code), Some(why));
            assert!(
                Unstarted::try_from(code).is_err()
                    && Unsteered::try_from(code).is_err()
                    && Reason::try_from(code).is_err()
            );
        }
    }

    fn log_ask(source: LogSource) -> LogAsk {
        LogAsk {
            source,
            channels: LogChannels {
                stdout: false,
                stderr: true,
            },
            cursor: LogCursor {
                stdout: None,
                stderr: Some(LogPos {
                    seq: 3,
                    no: 9,
                    byte: u64::MAX,
                }),
            },
            follow: true,
        }
    }

    /// A log lane's ask reads back as it was sent, the run's command's and a member's, each
    /// channel's place given or the oldest kept; one whose member is not its source's is never
    /// sent; and a frame that says anything else is the controller's error: a command's naming
    /// a member, no channel or a bit none has, a follow that is neither, a place of a flag none
    /// has or bytes past none, or a length off by one.
    #[test]
    fn a_log_ask_reads_back_and_one_amiss_is_refused() {
        let run = RunId([4; 32]);
        for source in [LogSource::Command, LogSource::Member(5)] {
            let ask = Ask::log(run, 9, log_ask(source));
            assert_eq!(ask.member, source.member());
            let frame = ask.frame().unwrap();
            assert_eq!(frame::peek(&frame).unwrap(), Some((Tag::Log, SCOPE + LOG_ASK)));
            assert_eq!(Ask::decode(&frame), Ok(ask.clone()));
            let oldest = Ask::log(
                run,
                9,
                LogAsk {
                    cursor: LogCursor::OLDEST,
                    channels: LogChannels::BOTH,
                    follow: false,
                    ..log_ask(source)
                },
            );
            assert_eq!(Ask::decode(&oldest.frame().unwrap()), Ok(oldest));
        }
        let astray = Ask {
            member: 2,
            ..Ask::log(run, 9, log_ask(LogSource::Member(5)))
        };
        assert!(astray.frame().is_err());
        let frame = Ask::log(run, 9, log_ask(LogSource::Command)).frame().unwrap();
        let at = FRAME_HEAD + SCOPE;
        let amiss = |change: &dyn Fn(&mut Vec<u8>)| {
            let mut bad = frame.clone();
            change(&mut bad);
            Ask::decode(&bad)
        };
        // A command's ask naming member 1.
        assert_eq!(amiss(&|b| b[at - 1] = 1), Err(Reason::Protocol));
        for channels in [0, 4, 7] {
            assert_eq!(amiss(&|b| b[at + 1] = channels), Err(Reason::Protocol));
        }
        assert_eq!(amiss(&|b| b[at + 2] = 2), Err(Reason::Protocol));
        // stdout's place: none, its bytes past none; then a flag none has.
        assert_eq!(amiss(&|b| b[at + 4] = 1), Err(Reason::Protocol));
        assert_eq!(amiss(&|b| b[at + 3] = 2), Err(Reason::Protocol));
        let reframed = |payload: &[u8]| {
            let mut out = Vec::new();
            frame::put(&mut out, Tag::Log, payload).unwrap();
            out
        };
        let payload = &frame[FRAME_HEAD..];
        assert_eq!(
            Ask::decode(&reframed(&payload[..payload.len() - 1])),
            Err(Reason::Protocol)
        );
        let mut longer = payload.to_vec();
        longer.push(0);
        assert_eq!(Ask::decode(&reframed(&longer)), Err(Reason::Protocol));
    }

    /// A log lane's records read back as they were written: output with its bytes, no text
    /// holds them, a gap with what it lost and why, a rotation with the oldest place kept; and
    /// what the driver holds them to agrees. Output of nothing, a kind, channel or cause none
    /// has, or a length off its kind's is none.
    #[test]
    fn a_log_record_reads_back_and_one_amiss_is_refused() {
        let pos = LogPos {
            seq: 2,
            no: 5,
            byte: 40,
        };
        for record in [
            LogRecord::Output {
                channel: Channel::Stderr,
                pos,
                at: 1_794_590_000_000_000_000,
                bytes: b"\x00\xff no newline".to_vec(),
            },
            LogRecord::Gap {
                channel: Channel::Stdout,
                pos,
                at: 1,
                lost: u64::MAX,
                why: Gapped::Behind,
            },
            LogRecord::Gap {
                channel: Channel::Stderr,
                pos,
                at: 2,
                lost: 7,
                why: Gapped::Unwritten,
            },
            LogRecord::Rotated { oldest: pos },
        ] {
            let mut bytes = Vec::new();
            record.write_to(&mut bytes).unwrap();
            assert!(LogRecord::check(&bytes), "{record:?}");
            assert_eq!(LogRecord::read(bytes.clone()), Ok(record.clone()));
            // Output is as long as its bytes: cut to its head alone it is none.
            let short = match record {
                LogRecord::Output { .. } => LOG_OUTPUT_HEAD,
                _ => bytes.len() - 1,
            };
            for cut in [short, 1] {
                assert!(!LogRecord::check(&bytes[..cut]), "{record:?} cut at {cut}");
                assert_eq!(LogRecord::read(bytes[..cut].to_vec()), Err(Reason::Protocol));
            }
        }
        let head = log_output_head(Channel::Stdout, pos, 3);
        assert!(!LogRecord::check(&head));
        assert_eq!(LogRecord::read(head.to_vec()), Err(Reason::Protocol));
        let mut output = head.to_vec();
        output.push(b'x');
        assert!(LogRecord::check(&output));
        for (at, byte) in [(0, 9), (1, 0), (1, 3)] {
            let mut bad = output.clone();
            bad[at] = byte;
            assert!(!LogRecord::check(&bad));
            assert_eq!(LogRecord::read(bad), Err(Reason::Protocol));
        }
        let mut gap = Vec::new();
        LogRecord::Gap {
            channel: Channel::Stdout,
            pos,
            at: 1,
            lost: 1,
            why: Gapped::Behind,
        }
        .write_to(&mut gap)
        .unwrap();
        *gap.last_mut().unwrap() = 3;
        assert!(!LogRecord::check(&gap));
        assert_eq!(LogRecord::read(gap), Err(Reason::Protocol));
    }

    /// A reader's cursor moves past what it had, for the channels it reads alone, and never
    /// back: past output's bytes, past a gap's record, to the oldest kept past a rotation.
    #[test]
    fn a_log_cursor_moves_past_what_its_reader_had() {
        let at = |seq, no, byte| LogPos { seq, no, byte };
        let stderr_only = LogChannels {
            stdout: false,
            stderr: true,
        };
        let from = LogCursor {
            stdout: Some(at(1, 0, 0)),
            stderr: None,
        };
        let output = LogRecord::Output {
            channel: Channel::Stderr,
            pos: at(1, 4, 10),
            at: 0,
            bytes: vec![b'e'; 5],
        };
        assert_eq!(
            from.past(&output, stderr_only),
            LogCursor {
                stdout: Some(at(1, 0, 0)),
                stderr: Some(at(1, 4, 15)),
            }
        );
        // Both read: a channel's place is past every record before, of either.
        let both = from.past(&output, LogChannels::BOTH);
        assert_eq!(both.stdout, Some(at(1, 4, 15)));
        let ahead = LogCursor {
            stdout: Some(at(9, 0, 0)),
            stderr: None,
        };
        assert_eq!(ahead.past(&output, LogChannels::BOTH).stdout, Some(at(9, 0, 0)));
        let gap = LogRecord::Gap {
            channel: Channel::Stdout,
            pos: at(2, 3, 0),
            at: 0,
            lost: 100,
            why: Gapped::Behind,
        };
        assert_eq!(gap.after(), Some(at(2, 4, 0)));
        let rotated = LogRecord::Rotated { oldest: at(6, 0, 0) };
        assert_eq!(
            LogCursor::OLDEST.past(&rotated, LogChannels::BOTH),
            LogCursor {
                stdout: Some(at(6, 0, 0)),
                stderr: Some(at(6, 0, 0)),
            }
        );
        assert_eq!(at(1, 2, u64::MAX).after(1), None);
    }

    /// A member's lane holds what it adds to its connection while it is open, its own windows
    /// alone, which its host charges: a steer read and a frame told at once.
    #[test]
    fn a_members_lane_holds_its_own_windows_alone() {
        assert_eq!(
            lane_added(),
            Some(Limits {
                streams: 1,
                receive: STEER_BYTES as u64,
                send: TOLD_BYTES as u64,
            })
        );
        assert_eq!(member_lane_bytes(), Some((STEER_BYTES + TOLD_BYTES) as u64));
    }

    /// Each exec's streams are its own: no two execs', and no member's, share a run.
    #[test]
    fn each_execs_streams_are_scoped_apart() {
        let run = RunId([1; 32]);
        assert_ne!(exec_run(run, 0), exec_run(run, 1));
        assert_ne!(exec_run(run, 0), run);
        assert_ne!(exec_run(run, 0), exec_run(RunId([2; 32]), 0));
    }
}
