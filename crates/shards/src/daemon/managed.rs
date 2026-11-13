//! A run's admitted, independently credited owner-output channels (D180).

mod drain;
mod execs;
mod log_lanes;
mod members;
#[cfg(test)]
mod terminal_order_tests;

pub(super) use drain::member_logs;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use sha2::{Digest as _, Sha256};
use shards_abi::run::{
    self,
    managed::{Channel, Event, Record, StreamState},
};
use shards_ipc::Stream;
use shards_ipc::fd::AsRawFd as _;
use shards_remote::{
    control,
    identity::{self, Fingerprint},
    managed::{Admitted, Control, Endpoint, Producer, Progress, Rejected, RunId, Scope, State, input},
};

use super::{Daemon, Disk, Threads, lock, log};
use crate::spec::LogRetention;

const METADATA: &str = "managed-output";

/// Whether the run whose directory is `dir` has a managed scope: its [`METADATA`], written as
/// the scope begins and kept.
pub(super) fn has_scope(dir: &Path) -> bool {
    dir.join(METADATA).exists()
}

/// The run's own key (D238, plan step 11), as its managed metadata names it: the `vm` line
/// of a run whose owner asked for its agent-control endpoint ([`CONTROL_ANNOTATION`]); none
/// for any other run.
pub(super) fn vm_key(dir: &Path) -> Option<Fingerprint> {
    let text = std::fs::read_to_string(dir.join(METADATA)).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("vm "))
        .and_then(|hex| crate::context::unhex32(hex.trim()))
}

const GENERATION: &str = "managed-generation";
// RFC 8446 §4: Handshake's uint24 length and four-byte type/length head.
const HANDSHAKE_BYTES: usize = (1 << 24) - 1 + 4;
/// The run's annotation naming the members whose stdin its owner writes (plan step 5, D231
/// decision §3.3): each `KIND:NAME` as the image's Agentfile declares it (`agent` or
/// `harness`), comma-separated. shards' own key, as `shards.egress` is a network's (D192):
/// an OCI runtime's options travel in annotations (OCI runtime-spec config.md, "Annotations").
/// A member it does not name has `/dev/null`: input is closed unless its owner asks for it.
pub(super) const STDIN_ANNOTATION: &str = "shards.stdin";
/// The run's annotation naming the members whose stdout and stderr its owner reads through
/// its endpoints (D240), each `KIND:NAME` as [`STDIN_ANNOTATION`] names them. A member it does
/// not name the daemon reads itself, into its member's log as it comes, so that none waits on
/// a reader (drain.rs): the members of a detached run, or of one whose client has gone.
pub(super) const OUTPUT_ANNOTATION: &str = "shards.output";
/// The most one input record carries: one `STDIN` frame to the member's init, which carries
/// at most what the guest reads of a stream at once (`run::CHUNK`), as a command's and an
/// exec's stdin frames do.
const INPUT_ROOM: usize = run::CHUNK;

/// The members a run's annotations give an input, by their declared index: each named
/// once, each one of the image's members that runs a program (`active`), refused by name
/// otherwise. None where the run names none.
pub(super) fn input_members(
    annotations: &[String],
    labels: &[String],
    active: &[u32],
) -> Result<Vec<u32>, String> {
    named_members(annotations, STDIN_ANNOTATION, "to read its stdin", labels, active)
}

/// The members whose output a run's owner reads ([`OUTPUT_ANNOTATION`]), as
/// [`input_members`] names them. None where the run names none: the daemon reads them all.
pub(super) fn output_members(
    annotations: &[String],
    labels: &[String],
    active: &[u32],
) -> Result<Vec<u32>, String> {
    named_members(annotations, OUTPUT_ANNOTATION, "to write output", labels, active)
}

/// The members annotation `annotation` names, by their declared index: each named once, each
/// one of the image's members that runs a program (`active`), refused by name otherwise.
fn named_members(
    annotations: &[String],
    annotation: &str,
    purpose: &str,
    labels: &[String],
    active: &[u32],
) -> Result<Vec<u32>, String> {
    let mut named = None;
    for kv in annotations {
        let (key, value) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
        if key == annotation {
            // Docker's MapOpts keeps a key's last value (daemon.rs `annotations`).
            named = Some(value);
        }
    }
    let Some(value) = named else {
        return Ok(Vec::new());
    };
    let mut members = Vec::new();
    for entry in value.split(',').map(str::trim) {
        let (kind, name) = entry
            .split_once(':')
            .filter(|(kind, name)| matches!(*kind, "agent" | "harness") && !name.is_empty())
            .ok_or_else(|| {
                format!("--annotation {annotation}={value}: {entry:?} is no agent:NAME or harness:NAME")
            })?;
        let label = format!("{kind} {name}");
        let member = labels
            .iter()
            .position(|l| *l == label)
            .and_then(|i| u32::try_from(i).ok())
            .ok_or_else(|| format!("--annotation {annotation}: the image declares no {kind} {name}"))?;
        if !active.contains(&member) {
            return Err(format!(
                "--annotation {annotation}: {kind} {name} runs no program {purpose}"
            ));
        }
        if members.contains(&member) {
            return Err(format!("--annotation {annotation}: {kind} {name} named twice"));
        }
        members.try_reserve(1).map_err(|e| e.to_string())?;
        members.push(member);
    }
    members.sort_unstable();
    Ok(members)
}

/// The run's annotation naming the members given a terminal of their own (D244, goal 58's
/// step 6), each `KIND:NAME` as [`STDIN_ANNOTATION`] names them: its stdio a terminal whose
/// master init holds, shown as its stdout, typed into through its stdin where its owner writes
/// that, and sized over its own lane of the run's agent-control endpoint, which it needs
/// ([`CONTROL_ANNOTATION`]).
pub(super) const TERMINAL_ANNOTATION: &str = "shards.terminal";

/// The members a run's annotations give a terminal of their own ([`TERMINAL_ANNOTATION`]), as
/// [`input_members`] names them: none where the run names none.
pub(super) fn terminal_members(
    annotations: &[String],
    labels: &[String],
    active: &[u32],
) -> Result<Vec<u32>, String> {
    named_members(
        annotations,
        TERMINAL_ANNOTATION,
        "on a terminal of its own",
        labels,
        active,
    )
}

/// The run's annotation asking for its agent-control endpoint (D238, plan step 11): `true`
/// serves every managed stream of the run as a lane of one endpoint of the microVM's own key, in
/// place of step 5's endpoint per stream, which `false`, or no such annotation, keeps. shards'
/// own key, as `shards.stdin` is ([`STDIN_ANNOTATION`]).
pub(super) const CONTROL_ANNOTATION: &str = "shards.control";

/// Whether a run's annotations ask for its agent-control endpoint: the key's last value, as
/// Docker's MapOpts keeps it, `true` or `false`, any other refused.
pub(super) fn control_asked(annotations: &[String]) -> Result<bool, String> {
    let last = annotations
        .iter()
        .filter_map(|kv| {
            let (key, value) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
            (key == CONTROL_ANNOTATION).then_some(value)
        })
        .next_back();
    match last {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(value) => Err(format!(
            "--annotation {CONTROL_ANNOTATION}={value}: neither true nor false"
        )),
    }
}

#[derive(Debug)]
pub(super) enum Failure {
    /// What the run asked of its managed members that its image does not give.
    Refused(String),
    Admission(Admission),
    Auth(Auth),
    Protocol(io::Error),
    Transport(io::Error),
    Endpoint(shards_remote::managed::Error),
    Closed,
}

#[derive(Debug)]
pub(super) enum Auth {
    UnknownContext,
    InvalidKey,
}

#[derive(Debug)]
pub(super) enum Admission {
    UnknownHeadroom,
    Unavailable,
    Insufficient { requested: u64, available: u64 },
    Overflow,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(Admission::Insufficient { requested, available }) => write!(
                out,
                "managed output admission: {requested} bytes requested, {available} available (retry after headroom returns)"
            ),
            Self::Refused(why) => out.write_str(why),
            Self::Admission(reason) => write!(out, "managed output admission: {reason:?}"),
            Self::Auth(reason) => write!(out, "managed output owner: {reason:?}"),
            Self::Protocol(error) => write!(out, "managed output protocol: {error}"),
            Self::Transport(error) => write!(out, "managed output transport: {error}"),
            Self::Endpoint(error) => std::fmt::Display::fmt(error, out),
            Self::Closed => out.write_str("managed output closed"),
        }
    }
}
impl std::error::Error for Failure {}
impl Failure {
    /// Whether it is a connection's end, its peer's (its VM's, as its run ends): its end of
    /// file, a pipe its peer closed, a reset; none of the daemon's own failures.
    pub(super) fn ended(&self) -> bool {
        match self {
            Self::Closed => true,
            Self::Transport(e) => matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
            ),
            _ => false,
        }
    }
}
impl From<Admission> for Failure {
    fn from(reason: Admission) -> Self {
        Self::Admission(reason)
    }
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::InvalidData => Self::Protocol(error),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionAborted => {
                Self::Closed
            }
            _ => Self::Transport(error),
        }
    }
}

/// A run's members as its capture takes them, each list by declared index, ascending: those
/// that run a program (`active`), those whose stdin their owner writes (`inputs`), those whose
/// output their owner reads (`owned`; the daemon reads the rest itself), and those on a
/// terminal of their own (`terminals`, D244).
#[derive(Clone, Copy)]
pub(super) struct Members<'a> {
    pub active: &'a [u32],
    pub inputs: &'a [u32],
    pub owned: &'a [u32],
    pub terminals: &'a [u32],
}

/// Where what the daemon reads of its members is kept and shown: kept in their logs as the
/// run's own log is kept (none for a run that keeps no log, `--log-driver none`), and shown
/// on the stderr of a client attached to the run.
pub(super) struct Shown {
    pub retention: Option<LogRetention>,
    pub to: Option<File>,
}

pub(super) struct Outputs {
    runs: Mutex<HashMap<String, Entry>>,
    budget: Arc<Mutex<Option<u64>>>,
}

impl Default for Outputs {
    fn default() -> Self {
        Self {
            runs: Mutex::default(),
            budget: Arc::new(Mutex::new(Some(0))),
        }
    }
}

enum Entry {
    Preparing,
    Serving(Arc<Owner>),
}

/// An ended run's managed scope, whose capture may still be keeping its members' last
/// records ([`Outputs::end`]).
pub(super) struct RunEnd(Arc<Owner>);

impl RunEnd {
    /// The run's end, `ended`: to be told now where its capture has ended, else told by its
    /// credit worker once it has.
    pub(super) fn tell_after(&self, ended: super::Ended) -> Option<super::Ended> {
        self.0.tell_after(ended)
    }
}

struct Charge {
    budget: Arc<Mutex<Option<u64>>>,
    bytes: Mutex<u64>,
}

impl Charge {
    fn reserve(budget: &Arc<Mutex<Option<u64>>>, bytes: u64) -> Result<Self, Admission> {
        let charge = Self {
            budget: Arc::clone(budget),
            bytes: Mutex::new(0),
        };
        charge.grow(bytes)?;
        Ok(charge)
    }

    fn grow(&self, extra: u64) -> Result<(), Admission> {
        let available = crate::build::host_available().ok_or(Admission::UnknownHeadroom)?;
        let mut held = lock(&self.bytes);
        let mut budget = lock(&self.budget);
        let used = budget.ok_or(Admission::Unavailable)?;
        let next = used.checked_add(extra).ok_or(Admission::Overflow)?;
        if next > available {
            return Err(Admission::Insufficient {
                requested: extra,
                available: available.saturating_sub(used),
            });
        }
        *held = held.checked_add(extra).ok_or(Admission::Overflow)?;
        *budget = Some(next);
        Ok(())
    }

    fn release(&self, bytes: u64) -> Result<(), Admission> {
        let mut held = lock(&self.bytes);
        let mut budget = lock(&self.budget);
        let Some((remaining, total)) = held
            .checked_sub(bytes)
            .zip(budget.and_then(|used| used.checked_sub(bytes)))
        else {
            *budget = None;
            return Err(Admission::Unavailable);
        };
        *held = remaining;
        *budget = Some(total);
        Ok(())
    }

    fn prepare_growth(&self, bytes: u64) -> Result<Growth<'_>, Admission> {
        self.grow(bytes)?;
        Ok(Growth {
            charge: self,
            state: GrowthState::Prepared(bytes),
        })
    }
}

struct Growth<'a> {
    charge: &'a Charge,
    state: GrowthState,
}

enum GrowthState {
    Prepared(u64),
    Committed,
}

impl Growth<'_> {
    fn extend(&mut self, bytes: u64) -> Result<(), Admission> {
        let GrowthState::Prepared(held) = self.state else {
            return Err(Admission::Unavailable);
        };
        let next = held.checked_add(bytes).ok_or(Admission::Overflow)?;
        if bytes > 0 {
            self.charge.grow(bytes)?;
        }
        self.state = GrowthState::Prepared(next);
        Ok(())
    }

    fn commit(mut self, replaced: u64) -> Result<(), Admission> {
        self.charge.release(replaced)?;
        self.state = GrowthState::Committed;
        Ok(())
    }
}

impl Drop for Growth<'_> {
    fn drop(&mut self) {
        if let GrowthState::Prepared(bytes) = self.state {
            let _ = self.charge.release(bytes);
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let held = *lock(&self.bytes);
        let mut budget = lock(&self.budget);
        *budget = budget.and_then(|used| used.checked_sub(held));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    End,
    Aborted(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    Serving,
    Draining(Terminal),
    Ended,
    // None means no canonical terminal was submitted before interruption.
    Aborted(Option<Terminal>),
}

#[derive(Clone, Copy)]
enum Resolve {
    Complete,
    Stop,
}

impl Life {
    fn drain(&mut self, terminal: Terminal) -> io::Result<()> {
        if !matches!(self, Self::Serving) {
            return Err(io::Error::other("capture scope ended"));
        }
        *self = Self::Draining(terminal);
        Ok(())
    }

    fn resolve(&mut self, state: State, event: Resolve) -> Option<Self> {
        let next = match (*self, state, event) {
            (Self::Draining(Terminal::End), State::Finished(_), _) => Self::Ended,
            (Self::Draining(terminal), State::Finished(_) | State::Aborted(_), _)
            | (Self::Draining(terminal), _, Resolve::Stop) => Self::Aborted(Some(terminal)),
            (Self::Serving, State::Aborted(_), _) | (Self::Serving, _, Resolve::Stop) => Self::Aborted(None),
            _ => return None,
        };
        *self = next;
        Some(next)
    }

    /// Its terminal record, formatted whole into `buffer` to be appended at once: the
    /// canonical errno it knows, then its state; nothing while it is not terminal.
    fn record(self, scope: Scope, at: Progress, buffer: &mut [u8; TERMINAL_MOST]) -> io::Result<&[u8]> {
        let (state, tail) = match self {
            Self::Ended => ("ended", "complete"),
            Self::Aborted(_) => ("aborted", "unknown"),
            _ => return Ok(&[]),
        };
        let mut rest = buffer.as_mut_slice();
        if let Self::Aborted(Some(Terminal::Aborted(errno))) = self {
            writeln!(
                rest,
                "member {} channel {} canonical aborted errno {errno}",
                scope.member, scope.channel as u8
            )?;
        }
        writeln!(
            rest,
            "member {} channel {} state {state} accepted {} consumed {} tail {tail}",
            scope.member, scope.channel as u8, at.accepted, at.consumed
        )?;
        let used = TERMINAL_MOST.saturating_sub(rest.len());
        buffer
            .get(..used)
            .ok_or_else(|| io::Error::other("managed output terminal record overflow"))
    }
}

/// The most bytes one channel's terminal record takes ([`Life::record`]): its canonical
/// errno line and its state line, their words as written, each number at its type's widest
/// in decimal (a u32 member, a u8 channel, an i32 errno with its sign, two u64 cursors), and
/// the longer of each word that varies (`aborted`, `complete`).
const TERMINAL_MOST: usize = {
    let member = u32::MAX.ilog10() as usize + 1;
    let channel = u8::MAX.ilog10() as usize + 1;
    let errno = i32::MIN.unsigned_abs().ilog10() as usize + 2;
    let cursor = u64::MAX.ilog10() as usize + 1;
    "member  channel  canonical aborted errno \n".len()
        + member
        + channel
        + errno
        + "member  channel  state  accepted  consumed  tail \n".len()
        + member
        + channel
        + "aborted".len()
        + 2 * cursor
        + "complete".len()
};

struct Output {
    scope: Scope,
    address: std::net::SocketAddr,
    control: Control,
    metadata: Arc<Mutex<Ledger>>,
    life: Mutex<Life>,
    credits: std::sync::Weak<Credits>,
    index: usize,
    learning: Mutex<Learning>,
    charge: Arc<Charge>,
}

struct Sample {
    began: Instant,
    end: u64,
    bytes: u64,
}
struct Learning {
    quantum: shards_registry::Quantum,
    sample: Option<Sample>,
    admitted: usize,
}

impl Output {
    fn observed(&self) {
        let at = self.control.progress().consumed;
        let mut learning = lock(&self.learning);
        if learning.sample.as_ref().is_some_and(|sample| sample.end <= at)
            && let Some(sample) = learning.sample.take()
        {
            learning
                .quantum
                .consumed(sample.began, Instant::now(), sample.bytes);
        }
    }

    fn capacity(&self, producer: &Producer) -> io::Result<usize> {
        let mut learning = lock(&self.learning);
        let desired = learning.quantum.desired();
        if desired > learning.admitted {
            let old = producer.storage_capacity();
            let extra = desired
                .checked_sub(learning.admitted)
                .and_then(|n| n.checked_add(desired))
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| io::Error::other("capture growth overflow"))?;
            // Keep the old daemon allocation charged while preparing its replacement,
            // plus the increased VM record allowance. This is logical accounting;
            // neither it nor owned Vec storage establishes parent/kernel entitlement.
            if let Ok(mut growth) = self.charge.prepare_growth(extra) {
                match producer.admit(desired) {
                    Ok(actual) => {
                        growth
                            .extend(u64::try_from(actual.saturating_sub(desired)).map_err(io::Error::other)?)
                            .map_err(|reason| io::Error::other(Failure::Admission(reason)))?;
                        growth
                            .commit(u64::try_from(old).map_err(io::Error::other)?)
                            .map_err(|reason| io::Error::other(Failure::Admission(reason)))?;
                        learning.admitted = desired;
                    }
                    // A window the endpoint cannot take now (storage it could not allocate,
                    // or a size or a record in flight it refuses) leaves the channel the
                    // window it has; the growth's charge goes back as it drops.
                    Err(
                        shards_remote::managed::Error::Admission(
                            shards_remote::managed::AdmissionError::Allocation
                            | shards_remote::managed::AdmissionError::Capacity,
                        )
                        | shards_remote::managed::Error::Closed,
                    ) => {}
                    Err(error) => return Err(io::Error::other(error)),
                }
            }
        }
        Ok(desired.min(learning.admitted))
    }

    fn resolve(&self, event: Resolve) -> Option<(Life, State)> {
        let mut life = lock(&self.life);
        // Stop must observe Finished before retiring Shared, even if the credit
        // worker has not yet published it. Life arbitrates that single winner.
        let state = self.control.state();
        life.resolve(state, event).map(|next| (next, state))
    }

    fn publish(&self, terminal: Option<(Life, State)>) {
        let Some((life, state)) = terminal else { return };
        let mut buffer = [0; TERMINAL_MOST];
        let result = life
            .record(self.scope, state.progress(), &mut buffer)
            .and_then(|record| lock(&self.metadata).append(record));
        if let Err(error) = result {
            // Logical consumption is sticky, but a failed append is not publication.
            // Retire this transport without holding metadata/Life/credit locks.
            self.stop_transport();
            log(format!(
                "managed output metadata member {} channel {}: {error}",
                self.scope.member, self.scope.channel as u8
            ));
        }
    }

    fn abort(&self) {
        let terminal = self.resolve(Resolve::Stop);
        self.stop_transport();
        self.publish(terminal);
    }

    fn stop_transport(&self) {
        if let Some(credits) = self.credits.upgrade() {
            credits.revoke(self.index);
        }
        // Logical terminal does not release ownership of the endpoint's driver.
        // Stop is unconditional and precedes fallible metadata I/O.
        self.control.abort();
    }

    fn finish(&self, terminal: Terminal) -> io::Result<()> {
        lock(&self.life).drain(terminal)?;
        // Shared invokes the wake inline; never hold Life across it. A concurrent
        // stop remains terminal, and remote finish cannot revive an aborted slot.
        self.control.finish().map_err(io::Error::other)
    }

    fn complete(&self) {
        self.observed();
        let terminal = self.resolve(Resolve::Complete);
        if matches!(terminal, Some((_, State::Aborted(_)))) {
            self.stop_transport();
        }
        self.publish(terminal);
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.abort();
    }
}

struct Owner {
    outputs: Vec<Option<Arc<Output>>>,
    /// Each member's input, by its index, where its owner asked for it.
    inputs: Vec<Option<Arc<Input>>>,
    /// What the daemon reads itself: the channels of every member whose output no owner
    /// reads ([`OUTPUT_ANNOTATION`]).
    drain: Option<Arc<drain::Drain>>,
    /// The run's agent-control endpoint, where the run asked for it ([`CONTROL_ANNOTATION`]).
    agent: Option<Agent>,
    stream: Stream,
    credits: Arc<Credits>,
    charge: Arc<Charge>,
    /// Each member's end as its init tells it (`MEMBER_ENDED`), for its own lanes (D238).
    ends: Arc<members::Ends>,
    /// Whether its capture has ended, and the run's end it holds until it has.
    told: Mutex<Told>,
}

/// How a run's end waits on its capture (D240): told once every record its capture brought is
/// kept, so that what its end brings (its waiters, `logs -f`, its removal, its restart) finds
/// its members' logs whole. Nothing here waits on a reader of them.
enum Told {
    /// Its credit worker is keeping what comes.
    Capturing,
    /// The run has ended, and its end waits for the capture's.
    Held(super::Ended),
    /// Its capture has ended: the run's end is told as it comes.
    Captured,
}

/// A run's agent-control endpoint as its owner holds it: where it is bound, and what ends it.
/// Its driver runs on a thread of its own; its key is the endpoint's alone ([`bind_agent`]).
struct Agent {
    address: std::net::SocketAddr,
    revoke: control::Revoke,
}

/// The run's agent-control endpoint (D238, plan step 11): a key of the microVM's own, made for
/// this run as its scope begins, which no template carries and only the endpoint holds; serving
/// `outputs` and `inputs` as lanes of one connection to the run's owner `key` alone, and the
/// execs its owner asks for in its members with `execs`' streams, bound on loopback as step 5's
/// endpoints are.
fn bind_agent(
    key: Fingerprint,
    outputs: &[(Scope, Admitted)],
    inputs: &[(input::Scope, Admitted)],
    execs: control::exec::Admissions,
) -> Result<(control::Endpoint, control::Served), Failure> {
    let vm = identity::Identity::generate().map_err(|e| Failure::Endpoint(e.into()))?;
    control::Endpoint::bind(
        &vm,
        key,
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        control::Streams {
            outputs,
            inputs,
            execs: Some(execs),
            sockets: None,
        },
    )
    .map_err(Failure::Endpoint)
}

/// What each exec's streams are admitted (D238): an output's record is what one of its
/// guest's output frames carries at most (`run::CHUNK`), its window that record with its
/// data frame's head, as a member's is; its stdin's is a member's stdin's, `input`. Their
/// handshake and datagrams are the connection's, `output`'s as the run's.
fn exec_admissions(output: Admitted, input: Admitted) -> io::Result<control::exec::Admissions> {
    let transport = run::CHUNK
        .checked_add(shards_remote::managed::DATA_HEAD)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| io::Error::other("an exec's window overflows"))?;
    Ok(control::exec::Admissions {
        output: Admitted {
            record_bytes: run::CHUNK,
            transport_bytes: transport,
            ..output
        },
        input,
    })
}

impl Owner {
    fn abort(&self) {
        self.credits.close();
        shards_ipc::end(&self.stream);
        if let Some(drain) = &self.drain {
            drain.close();
        }
        self.end_owned();
    }

    /// The run has ended (its DONE, or its VM's end): what owners read through their
    /// endpoints and lanes ends with it, as its inputs do. What the daemon drains goes on to
    /// the end its capture brings, as its last records may still be on their way: they come
    /// on the capture's own stream, the run's end on its VM's link.
    fn end_run(&self) {
        self.end_owned();
    }

    /// Its capture ended before its channels did (its stream's end, or a failure of it), once
    /// nothing more is read of it: what owners read ends aborted; what came of the drained
    /// channels is kept, and then its credit worker ends ([`Credits::end`]).
    fn cut(&self) {
        self.end_owned();
        self.credits
            .end(|index| self.drain.as_ref().is_some_and(|drain| drain.drains(index)));
        shards_ipc::end(&self.stream);
    }

    fn end_owned(&self) {
        if let Some(agent) = &self.agent {
            agent.revoke.revoke();
        }
        self.ends.close();
        for output in self.outputs.iter().flatten() {
            output.abort();
        }
        for input in self.inputs.iter().flatten() {
            input.record();
            input.control.abort();
            input.close();
        }
    }

    /// The run has ended with `ended`: to be told now where its capture has ended, else held
    /// for it ([`Told`]).
    fn tell_after(&self, ended: super::Ended) -> Option<super::Ended> {
        let mut told = lock(&self.told);
        if matches!(*told, Told::Capturing) {
            *told = Told::Held(ended);
            return None;
        }
        Some(ended)
    }

    /// Its capture has ended, every record it brought kept: the run's end it held, if any.
    fn captured(&self) -> Option<super::Ended> {
        match std::mem::replace(&mut *lock(&self.told), Told::Captured) {
            Told::Held(ended) => Some(ended),
            Told::Capturing | Told::Captured => None,
        }
    }
}

/// A member's input, which its owner writes on its endpoint ([`input`]): the endpoint's
/// control, and the member's input connection, which its VM hands over once its init's hello
/// has come (`MANAGED_INPUT`), relayed to by a thread of its own.
struct Input {
    scope: input::Scope,
    address: std::net::SocketAddr,
    control: input::Control,
    guest: Mutex<Guest>,
    arrived: Condvar,
    /// The run's metadata record, which the input's end is appended to, once.
    metadata: Arc<Mutex<Ledger>>,
    recorded: std::sync::atomic::AtomicBool,
}

/// The most bytes an input's terminal record takes ([`Input::record`]): its words as written,
/// each number at its type's widest in decimal (a u32 member, a u8 channel, a u64 count and an
/// i32 errno with its sign), and the longest of the words that vary.
const INPUT_TERMINAL_MOST: usize = {
    let member = u32::MAX.ilog10() as usize + 1;
    let channel = u8::MAX.ilog10() as usize + 1;
    let count = u64::MAX.ilog10() as usize + 1;
    let errno = i32::MIN.unsigned_abs().ilog10() as usize + 2;
    "member  channel  state  written  errno \n".len() + member + channel + "aborted".len() + count + errno
};

/// The member's input connection, as far as the daemon has it.
enum Guest {
    Awaited,
    /// Here: the relay writes a copy of its own; this one ends it.
    Here(Stream),
    /// Ended, or never to come: the input ended or the run's scope did.
    Gone,
}

impl Input {
    /// The member's input connection has come: refused where one came before, or none will.
    fn arrive(&self, conn: Stream) -> io::Result<()> {
        let mut guest = lock(&self.guest);
        if !matches!(*guest, Guest::Awaited) {
            shards_ipc::end(&conn);
            return Err(io::Error::other(
                "a member's input connection that is not awaited",
            ));
        }
        *guest = Guest::Here(conn);
        drop(guest);
        self.arrived.notify_all();
        Ok(())
    }

    /// Waits for the member's input connection: a copy of it, none where it will not come.
    fn wait(&self) -> Option<Stream> {
        let mut guest = lock(&self.guest);
        loop {
            match &*guest {
                Guest::Here(conn) => return conn.try_clone().ok(),
                Guest::Gone => return None,
                Guest::Awaited => {
                    guest = self
                        .arrived
                        .wait(guest)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        }
    }

    /// Appends the input's end to the run's metadata record, once: `closed` or `aborted` with
    /// the bytes the member's stdin took and its errno, as its init told it; `revoked`, with
    /// the bytes taken of its owner and none told, where the run's scope ended first.
    fn record(&self) {
        if self.recorded.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let (state, count, errno) = match self.control.state() {
            input::State::Ended { written, end } => match end {
                run::managed::InputEnd::Closed => ("closed", written, 0),
                run::managed::InputEnd::Aborted(errno) => ("aborted", written, errno),
            },
            input::State::Open { at } | input::State::Ending { at, .. } => ("revoked", at, 0),
            input::State::Revoked => ("revoked", 0, 0),
        };
        let mut buffer = [0u8; INPUT_TERMINAL_MOST];
        let mut rest = buffer.as_mut_slice();
        let written = writeln!(
            rest,
            "member {} channel {} state {state} written {count} errno {errno}",
            self.scope.member,
            input::CHANNEL
        )
        .map(|()| INPUT_TERMINAL_MOST.saturating_sub(rest.len()));
        let result = written.and_then(|used| {
            let line = buffer
                .get(..used)
                .ok_or_else(|| io::Error::other("managed input terminal record overflow"))?;
            lock(&self.metadata).append(line)
        });
        if let Err(error) = result {
            log(format!(
                "managed input metadata member {}: {error}",
                self.scope.member
            ));
        }
    }

    /// Ends the member's input connection, so that a record held for a member that does not
    /// read it lets its relay go; once ended, none comes again.
    fn close(&self) {
        let mut guest = lock(&self.guest);
        if let Guest::Here(conn) = &*guest {
            shards_ipc::end(conn);
        }
        *guest = Guest::Gone;
        drop(guest);
        self.arrived.notify_all();
    }
}

/// Takes the owner's records to the member's input connection as they come, each as one
/// `STDIN` frame, and its end as an empty one, which init writes to the member's stdin
/// exactly; told each is in the connection, the endpoint takes the next. A record the
/// connection refuses is not delivered: the member's init tells its input's end, which
/// [`Outputs::input_ended`] gives the endpoint.
fn relay_input(input: &Input, relay: &input::Relay) {
    let mut guest: Option<Stream> = None;
    loop {
        match relay.next() {
            input::Next::Record { bytes, .. } => {
                if guest.is_none() {
                    guest = input.wait();
                }
                let Some(conn) = guest.as_ref() else {
                    continue;
                };
                let Ok(len) = u32::try_from(bytes.len()) else {
                    input.close();
                    continue;
                };
                let header = run::header(run::kind::STDIN, len);
                match (&*conn)
                    .write_all(&header)
                    .and_then(|()| (&*conn).write_all(&bytes))
                {
                    Ok(()) => relay.delivered(bytes),
                    Err(_) => guest = None,
                }
            }
            input::Next::End => {
                if guest.is_none() {
                    guest = input.wait();
                }
                if let Some(conn) = guest.as_ref() {
                    let _ = (&*conn).write_all(&run::header(run::kind::STDIN, 0));
                }
            }
            input::Next::Abort(_) => input.close(),
            input::Next::Gone => {
                input.close();
                return;
            }
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Clone, Copy)]
enum After {
    More,
    Terminal(Terminal),
}

enum CreditSlot {
    Inactive,
    Waiting,
    Granted {
        capacity: usize,
        began: Instant,
        bytes: Vec<u8>,
    },
    Captured(Vec<u8>, After),
    Sending,
    Draining,
    Ended,
    Revoked,
    RevokedGranted {
        capacity: usize,
        bytes: Vec<u8>,
    },
    Receiving,
    RevokedReceiving,
}

enum QueueLife {
    Open,
    /// Its capture brings nothing more: what it brought of the drained channels is still
    /// kept, and nothing more is granted (D240).
    Ending,
    Closed,
}
struct Queue {
    slots: Vec<CreditSlot>,
    next: usize,
    life: QueueLife,
}
struct Credits {
    state: Mutex<Queue>,
    changed: Condvar,
}
enum Action {
    Grant(usize),
    Submit(usize, Vec<u8>, After),
    Complete(usize),
}

impl Credits {
    fn revoke(&self, index: usize) {
        let mut state = lock(&self.state);
        if let Some(slot) = state.slots.get_mut(index) {
            *slot = match std::mem::replace(slot, CreditSlot::Revoked) {
                CreditSlot::Granted { capacity, bytes, .. } => CreditSlot::RevokedGranted { capacity, bytes },
                CreditSlot::Receiving => CreditSlot::RevokedReceiving,
                _ => CreditSlot::Revoked,
            };
        }
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.life = QueueLife::Closed;
        for slot in &mut state.slots {
            *slot = CreditSlot::Ended;
        }
        self.changed.notify_all();
    }

    /// Its capture brings nothing more: each record it brought of a channel `kept` says the
    /// daemon keeps itself waits still to be kept; every other slot ends, a credit being
    /// granted among them, which then goes nowhere.
    fn end(&self, kept: impl Fn(usize) -> bool) {
        let mut state = lock(&self.state);
        if !matches!(state.life, QueueLife::Open) {
            return;
        }
        state.life = QueueLife::Ending;
        for (index, slot) in state.slots.iter_mut().enumerate() {
            let waits = kept(index) && matches!(slot, CreditSlot::Captured(..));
            if !waits && !matches!(slot, CreditSlot::Inactive) {
                *slot = CreditSlot::Ended;
            }
        }
        self.changed.notify_all();
    }

    fn wake(&self) {
        // The predicate is read under this same mutex in next. Shared publishers
        // release Shared before this callback, so credit -> Shared has no reverse edge.
        let _state = lock(&self.state);
        self.changed.notify_all();
    }

    fn begin_record(&self, index: usize, len: usize) -> io::Result<(usize, Instant, Vec<u8>)> {
        let mut state = lock(&self.state);
        let slot = state
            .slots
            .get_mut(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown capture route"))?;
        let receiving = match slot {
            CreditSlot::Granted { capacity, .. } if len <= *capacity => CreditSlot::Receiving,
            CreditSlot::RevokedGranted { capacity, .. } if len <= *capacity => CreditSlot::RevokedReceiving,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "capture record exceeds issued channel credit",
                ));
            }
        };
        match std::mem::replace(slot, receiving) {
            CreditSlot::Granted {
                capacity,
                began,
                bytes,
            } => Ok((capacity, began, bytes)),
            CreditSlot::RevokedGranted { capacity, bytes } => Ok((capacity, Instant::now(), bytes)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture buffer ownership",
            )),
        }
    }

    fn captured(&self, index: usize, bytes: Vec<u8>, after: After) -> io::Result<()> {
        let mut state = lock(&self.state);
        if let Some(slot @ CreditSlot::RevokedReceiving) = state.slots.get_mut(index) {
            *slot = CreditSlot::Revoked;
            return Ok(());
        }
        let slot = state
            .slots
            .get_mut(index)
            .filter(|slot| matches!(slot, CreditSlot::Receiving))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "capture record has no channel credit")
            })?;
        *slot = CreditSlot::Captured(bytes, after);
        self.changed.notify_all();
        Ok(())
    }

    fn next(&self, observe: impl Fn(usize) -> io::Result<State>) -> io::Result<Option<Action>> {
        let mut state = lock(&self.state);
        loop {
            if matches!(state.life, QueueLife::Closed)
                || state.slots.iter().all(|s| {
                    matches!(
                        s,
                        CreditSlot::Inactive
                            | CreditSlot::Ended
                            | CreditSlot::Revoked
                            | CreditSlot::RevokedGranted { .. }
                            | CreditSlot::RevokedReceiving
                    )
                })
            {
                return Ok(None);
            }
            // An ending capture's last records are kept, and nothing more is granted.
            let ending = matches!(state.life, QueueLife::Ending);
            if ending && !state.slots.iter().any(|s| matches!(s, CreditSlot::Captured(..))) {
                return Ok(None);
            }
            let len = state.slots.len();
            if len == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "capture has no admitted channels",
                ));
            }
            for step in 0..len {
                let index = state.next.checked_add(step).map(|n| n % len).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "capture scheduler overflow")
                })?;
                let Some(slot) = state.slots.get_mut(index) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "capture scheduler index",
                    ));
                };
                if !matches!(
                    slot,
                    CreditSlot::Waiting | CreditSlot::Captured(..) | CreditSlot::Draining
                ) || (ending && !matches!(slot, CreditSlot::Captured(..)))
                {
                    continue;
                }
                // Observation releases Shared before an action leaves this credit
                // guard. Life and metadata are touched only by the worker afterward.
                let remote = observe(index)?;
                let action = match (std::mem::replace(slot, CreditSlot::Sending), remote) {
                    (CreditSlot::Waiting, State::Ready(_)) => Some(Action::Grant(index)),
                    (CreditSlot::Captured(bytes, after), State::Ready(_) | State::Capturing(_)) => {
                        Some(Action::Submit(index, bytes, after))
                    }
                    (CreditSlot::Draining, State::Finished(_))
                    | (
                        CreditSlot::Waiting | CreditSlot::Captured(..) | CreditSlot::Draining,
                        State::Aborted(_),
                    ) => {
                        *slot = CreditSlot::Ended;
                        Some(Action::Complete(index))
                    }
                    (old, _) => {
                        *slot = old;
                        None
                    }
                };
                if action.is_some() {
                    state.next = index.checked_add(1).map(|n| n % len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "capture scheduler overflow")
                    })?;
                    return Ok(action);
                }
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Outputs {
    pub(super) fn abort_member(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        let abort = run::managed::Abort::decode(bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let owner = match lock(&self.runs).get(id) {
            Some(Entry::Serving(owner)) => Arc::clone(owner),
            _ => return Err(io::Error::other("managed run is not registered")),
        };
        let first = usize::try_from(abort.member)
            .ok()
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| io::Error::other("invalid aborted member"))?;
        let end = first
            .checked_add(2)
            .ok_or_else(|| io::Error::other("member overflow"))?;
        let outputs = owner
            .outputs
            .get(first..end)
            .ok_or_else(|| io::Error::other("unknown aborted member"))?;
        let drained = owner
            .drain
            .as_ref()
            .filter(|drain| drain.drains_member(abort.member));
        if outputs.iter().all(Option::is_none) && drained.is_none() {
            return Err(io::Error::other("aborted member is inactive"));
        }
        for output in outputs.iter().flatten() {
            output.abort();
        }
        if let Some(drain) = drained {
            for index in first..end {
                owner.credits.revoke(index);
            }
            drain.abort_member(abort.member, abort.errno);
        }
        Ok(())
    }

    pub(super) fn published(&self, id: &str) -> Vec<[String; 3]> {
        match lock(&self.runs).get(id) {
            Some(Entry::Serving(owner)) if owner.agent.is_some() => owner
                .agent
                .iter()
                .map(|agent| {
                    [
                        "agent-control/udp".into(),
                        agent.address.to_string(),
                        "authenticated agent control".into(),
                    ]
                })
                .collect(),
            Some(Entry::Serving(owner)) => owner
                .outputs
                .iter()
                .flatten()
                .filter(|output| matches!(*lock(&output.life), Life::Serving | Life::Draining(_)))
                .map(|output| {
                    [
                        format!(
                            "managed-{}-{}/udp",
                            output.scope.member, output.scope.channel as u8
                        ),
                        output.address.to_string(),
                        "authenticated owner output".into(),
                    ]
                })
                .chain(
                    owner
                        .inputs
                        .iter()
                        .flatten()
                        .filter(|input| !matches!(input.control.state(), input::State::Revoked))
                        .map(|input| {
                            [
                                format!("managed-{}-{}/udp", input.scope.member, input::CHANNEL),
                                input.address.to_string(),
                                "authenticated owner input".into(),
                            ]
                        }),
                )
                .collect(),
            _ => Vec::new(),
        }
    }

    fn input_of(&self, id: &str, member: u32) -> io::Result<Arc<Input>> {
        let owner = match lock(&self.runs).get(id) {
            Some(Entry::Serving(owner)) => Arc::clone(owner),
            _ => return Err(io::Error::other("managed run is not registered")),
        };
        usize::try_from(member)
            .ok()
            .and_then(|m| owner.inputs.get(m))
            .and_then(Option::as_ref)
            .map(Arc::clone)
            .ok_or_else(|| io::Error::other(format!("member {member} has no input")))
    }

    /// Whether run `id`'s member `member` has an input whose connection has not come: on
    /// Windows, before a pipe is made for it.
    #[cfg(windows)]
    pub(super) fn awaits_input(&self, id: &str, member: u32) -> bool {
        self.input_of(id, member)
            .is_ok_and(|input| matches!(*lock(&input.guest), Guest::Awaited))
    }

    /// Member `member`'s input connection, as its VM handed it over: its relay writes to it.
    /// One the run does not await is ended, and its init tells its input's end.
    pub(super) fn input_arrived(&self, id: &str, member: u32, conn: Stream) -> io::Result<()> {
        match self.input_of(id, member) {
            Ok(input) => input.arrive(conn),
            Err(e) => {
                shards_ipc::end(&conn);
                Err(e)
            }
        }
    }

    /// A member's end, as its init told it (`MEMBER_ENDED`, D231): kept in its run's end table
    /// and told its lanes (D238).
    pub(super) fn member_ended(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        let ended = run::members::Ended::decode(bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a malformed member end"))?;
        let owner = match lock(&self.runs).get(id) {
            Some(Entry::Serving(owner)) => Arc::clone(owner),
            _ => return Err(io::Error::other("managed run is not registered")),
        };
        owner.ends.told(ended.member, ended.status)
    }

    /// A member's input's end, as its init told it (`MANAGED_INPUT_ENDED`): its owner told,
    /// and its connection ended here too.
    pub(super) fn input_ended(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        let ended = run::managed::InputEnded::decode(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let input = self.input_of(id, ended.member)?;
        input.control.ended(ended.written, ended.end);
        input.record();
        input.close();
        Ok(())
    }

    fn begin(&self, id: &str) -> io::Result<()> {
        let mut runs = lock(&self.runs);
        if runs.contains_key(id) {
            return Err(io::Error::other("managed run already has an output scope"));
        }
        runs.try_reserve(1).map_err(io::Error::other)?;
        runs.insert(id.to_owned(), Entry::Preparing);
        Ok(())
    }

    /// Run `id`'s scope goes now, whole: its run did not start, or its scope could not be
    /// made.
    pub(super) fn abort(&self, id: &str) {
        let removed = lock(&self.runs).remove(id);
        if let Some(Entry::Serving(owner)) = removed {
            owner.abort();
        }
    }

    /// Run `id` has ended (its DONE, or its VM's end): what its owners read ends, and what the
    /// daemon drains of its members goes on to its capture's own end, which its end is then
    /// told after ([`RunEnd::tell_after`]). None for a run with no managed scope.
    pub(super) fn end(&self, id: &str) -> Option<RunEnd> {
        let removed = lock(&self.runs).remove(id);
        match removed {
            Some(Entry::Serving(owner)) => {
                owner.end_run();
                Some(RunEnd(owner))
            }
            Some(Entry::Preparing) | None => None,
        }
    }

    pub(super) fn close(&self) {
        let runs = std::mem::take(&mut *lock(&self.runs));
        for entry in runs.into_values() {
            if let Entry::Serving(owner) = entry {
                owner.abort();
            }
        }
    }
}

impl<D: Disk> Daemon<D> {
    /// The daemon's end of run `id`'s managed capture failed as `e` says (`what`: its credits
    /// or its records). A connection's end ([`Failure::ended`]) is its VM's, as its run ends,
    /// and logged alone: its run's end is its VM's to say. Any other is the daemon's own
    /// failure, which leaves the run's members no way on: logged, and the run stopped as it
    /// stops, so that no run waits for its `STOP` on a failure its daemon saw (D258).
    pub(super) fn capture_failed(&self, id: &str, what: &str, e: &Failure) {
        if e.ended() {
            log(format!("container {id}: {what}: {e}"));
            return;
        }
        log(format!("container {id}: {what} failed, which stops it: {e}"));
        self.stop_for_failure(id);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn managed_capture<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        client: &str,
        labels: &[String],
        members: Members<'_>,
        shown: Shown,
        agent_control: bool,
        setup: &mut Vec<Vec<u8>>,
    ) -> Result<Stream, Failure> {
        self.managed.begin(id)?;
        let result =
            self.start_managed_capture(threads, id, client, labels, members, shown, agent_control, setup);
        if result.is_err() {
            self.managed.abort(id);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn start_managed_capture<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        client: &str,
        labels: &[String],
        members: Members<'_>,
        shown: Shown,
        agent_control: bool,
        setup: &mut Vec<Vec<u8>>,
    ) -> Result<Stream, Failure> {
        let Members {
            active,
            inputs,
            owned,
            terminals,
        } = members;
        // A member's own terminal is sized over its own lane of the run's endpoint alone.
        if !terminals.is_empty() && !agent_control {
            return Err(Failure::Refused(format!(
                "--annotation {TERMINAL_ANNOTATION}: a terminal of a member's own is sized over the run's \
                 agent control, which --annotation {CONTROL_ANNOTATION}=true asks for"
            )));
        }
        if terminals.windows(2).any(|w| w.first() >= w.get(1))
            || terminals.iter().any(|m| !active.contains(m))
        {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "invalid trusted terminal member plan").into(),
            );
        }
        let members = u32::try_from(labels.len()).map_err(io::Error::other)?;
        let count = labels
            .len()
            .checked_mul(2)
            .filter(|n| *n != 0)
            .ok_or_else(|| io::Error::other("invalid managed channel count"))?;
        if active.is_empty()
            || active.windows(2).any(|w| w.first() >= w.get(1))
            || active.iter().any(|id| *id >= members)
        {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "invalid trusted active member plan").into(),
            );
        }
        if inputs.windows(2).any(|w| w.first() >= w.get(1)) || inputs.iter().any(|m| !active.contains(m)) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "invalid trusted input member plan").into(),
            );
        }
        if owned.windows(2).any(|w| w.first() >= w.get(1)) || owned.iter().any(|m| !active.contains(m)) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "invalid trusted output owner plan").into(),
            );
        }
        let active_count = active
            .len()
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("active channel count overflow"))?;
        let capacity = record_capacity(labels, active)?;
        let transport = capacity
            .checked_add(shards_remote::managed::DATA_HEAD)
            .ok_or_else(|| io::Error::other("capture window overflow"))?;
        let per_channel = [
            capacity,
            capacity,
            transport,
            HANDSHAKE_BYTES,
            shards_remote::managed::CONTROL_BYTES,
            shards_remote::managed::DATAGRAM_BYTES,
            shards_remote::managed::DATAGRAM_BYTES,
            size_of::<Output>(),
        ]
        .into_iter()
        .try_fold(0u64, |sum, n| sum.checked_add(u64::try_from(n).ok()?))
        .ok_or_else(|| io::Error::other("capture reservation overflow"))?;
        let structural = count
            .checked_mul(
                size_of::<CreditSlot>()
                    + size_of::<StreamState>()
                    + size_of::<Option<Arc<Output>>>()
                    + size_of::<Option<Arc<Producer>>>(),
            )
            .and_then(|n| {
                labels
                    .iter()
                    .try_fold(n, |sum, label| sum.checked_add(label.len() + size_of::<String>()))
            })
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| io::Error::other("capture structure overflow"))?;
        // Each input: its one record, what its endpoint sends and holds of its owner's stream,
        // its handshake and datagrams, and its state.
        let per_input = [
            INPUT_ROOM,
            input::TAKEN_BYTES,
            input::RECORD_HEAD + INPUT_ROOM + input::END_BYTES,
            HANDSHAKE_BYTES,
            shards_remote::managed::DATAGRAM_BYTES,
            shards_remote::managed::DATAGRAM_BYTES,
            size_of::<Input>() + size_of::<Option<Arc<Input>>>(),
        ]
        .into_iter()
        .try_fold(0u64, |sum, n| sum.checked_add(u64::try_from(n).ok()?))
        .ok_or_else(|| io::Error::other("input reservation overflow"))?;
        let reservation = per_channel
            .checked_mul(u64::try_from(active_count).map_err(io::Error::other)?)
            .and_then(|n| n.checked_add(structural))
            .and_then(|n| n.checked_add(per_input.checked_mul(u64::try_from(inputs.len()).ok()?)?))
            .ok_or_else(|| io::Error::other("capture reservation overflow"))?;
        let charge = Charge::reserve(&self.managed.budget, reservation)?;
        let identity = crate::context::identity(&self.home).map_err(io::Error::other)?;
        let key = owner_key(&self.home, client, identity.fingerprint())?;
        let dir = lock(&self.containers).dir(id);
        let generation = generation(&dir)?;
        let run = RunId(Sha256::digest(id.as_bytes()).into());
        let admitted = Admitted {
            record_bytes: capacity,
            transport_bytes: u64::try_from(transport).map_err(io::Error::other)?,
            handshake_bytes: HANDSHAKE_BYTES,
            datagram_bytes: shards_remote::managed::DATAGRAM_BYTES,
        };
        let input_admitted = Admitted {
            record_bytes: INPUT_ROOM,
            transport_bytes: u64::try_from(input::TAKEN_BYTES).map_err(io::Error::other)?,
            handshake_bytes: HANDSHAKE_BYTES,
            datagram_bytes: shards_remote::managed::DATAGRAM_BYTES,
        };
        // Each stream with where it is served, and its own endpoint where it has one; the run's
        // agent-control endpoint serves them all where the run asked for it. An owner's are its
        // members' named for it alone (`owned`, as `inputs` are): the rest the daemon drains.
        let mut staged = Vec::new();
        staged.try_reserve_exact(active_count).map_err(io::Error::other)?;
        let mut staged_inputs = Vec::new();
        staged_inputs
            .try_reserve_exact(inputs.len())
            .map_err(io::Error::other)?;
        let mut agent = None;
        if agent_control {
            let mut lanes = Vec::new();
            lanes.try_reserve_exact(active_count).map_err(io::Error::other)?;
            for &member in owned {
                for channel in [Channel::Stdout, Channel::Stderr] {
                    let scope = Scope {
                        run,
                        generation,
                        member,
                        channel,
                    };
                    lanes.push((scope, admitted));
                }
            }
            let mut input_lanes = Vec::new();
            input_lanes
                .try_reserve_exact(inputs.len())
                .map_err(io::Error::other)?;
            for &member in inputs {
                let scope = input::Scope {
                    run,
                    generation,
                    member,
                };
                input_lanes.push((scope, input_admitted));
            }
            let exec_admitted = exec_admissions(admitted, input_admitted)?;
            let (endpoint, served) = bind_agent(key, &lanes, &input_lanes, exec_admitted)?;
            // One driver's socket, and the waker every stream's control shares.
            let mut kernel = endpoint.kernel_buffer_bytes().map_err(Failure::Endpoint)?;
            if let Some((_, control)) = served.outputs.first() {
                kernel = kernel
                    .checked_add(control.kernel_buffer_bytes().map_err(Failure::Endpoint)?)
                    .ok_or_else(|| io::Error::other("agent control kernel queue overflow"))?;
            }
            charge.grow(kernel)?;
            let control::Served {
                outputs: served_outputs,
                inputs: served_inputs,
                revoke,
                execs: starts,
                members: member_lanes,
                // A daemon's run asks no socket lane (D251): its endpoint refuses each.
                sockets: _,
            } = served;
            for ((scope, _), (producer, control)) in lanes.into_iter().zip(served_outputs) {
                let excess = u64::try_from(producer.storage_capacity().saturating_sub(capacity))
                    .map_err(io::Error::other)?;
                if excess > 0 {
                    charge.grow(excess)?;
                }
                staged.push((scope, endpoint.address, None, producer, control));
            }
            for ((scope, _), (relay, control)) in input_lanes.into_iter().zip(served_inputs) {
                staged_inputs.push((scope, endpoint.address, None, relay, control));
            }
            agent = Some((
                endpoint,
                revoke,
                starts.map(|starts| (starts, exec_admitted)),
                member_lanes,
            ));
        } else {
            for &member in owned {
                for channel in [Channel::Stdout, Channel::Stderr] {
                    let scope = Scope {
                        run,
                        generation,
                        member,
                        channel,
                    };
                    let (endpoint, producer, control) =
                        Endpoint::bind(&identity, key, scope, admitted).map_err(Failure::Endpoint)?;
                    let kernel = endpoint
                        .kernel_buffer_bytes()
                        .map_err(io::Error::other)?
                        .checked_add(control.kernel_buffer_bytes().map_err(Failure::Endpoint)?)
                        .ok_or_else(|| io::Error::other("capture kernel queue overflow"))?;
                    charge.grow(kernel)?;
                    let excess = u64::try_from(producer.storage_capacity().saturating_sub(capacity))
                        .map_err(io::Error::other)?;
                    if excess > 0 {
                        charge.grow(excess)?;
                    }
                    staged.push((scope, endpoint.address, Some(endpoint), producer, control));
                }
            }
            for &member in inputs {
                let scope = input::Scope {
                    run,
                    generation,
                    member,
                };
                let (endpoint, relay, control) = input::Endpoint::bind(&identity, key, scope, input_admitted)
                    .map_err(Failure::Endpoint)?;
                let kernel = endpoint
                    .kernel_buffer_bytes()
                    .map_err(io::Error::other)?
                    .checked_add(control.kernel_buffer_bytes().map_err(Failure::Endpoint)?)
                    .ok_or_else(|| io::Error::other("input kernel queue overflow"))?;
                charge.grow(kernel)?;
                staged_inputs.push((scope, endpoint.address, Some(endpoint), relay, control));
            }
        }
        #[cfg(unix)]
        let (stream, peer) = Stream::pair()?;
        #[cfg(windows)]
        let (stream, peer) = Stream::plain_pair()?;
        charge.grow(
            ipc_buffers(&stream)?
                .checked_add(ipc_buffers(&peer)?)
                .ok_or_else(|| io::Error::other("capture IPC queue overflow"))?,
        )?;
        let charge = Arc::new(charge);
        let mut metadata = private_record(&dir.join(METADATA))?;
        writeln!(
            metadata,
            "shards managed output 1\nserver {}\nowner {}\nrun {}\ngeneration {}\ncapacity {}",
            identity::hex(&identity.fingerprint()),
            identity::hex(&key),
            id,
            generation,
            capacity
        )?;
        match &agent {
            // Its controller dials the one endpoint and pins its key (D238).
            Some((endpoint, ..)) => writeln!(
                metadata,
                "control {}\nvm {}",
                endpoint.address,
                identity::hex(&endpoint.fingerprint)
            )?,
            None => {
                for (scope, address, ..) in &staged {
                    writeln!(
                        metadata,
                        "member {} channel {} address {}",
                        scope.member, scope.channel as u8, address
                    )?;
                }
                for (scope, address, ..) in &staged_inputs {
                    writeln!(
                        metadata,
                        "member {} channel {} address {}",
                        scope.member,
                        input::CHANNEL,
                        address
                    )?;
                }
            }
        }
        metadata.sync_all()?;
        let metadata = shared_metadata(metadata)?;
        let mut drained = Vec::new();
        drained
            .try_reserve_exact(active.len())
            .map_err(io::Error::other)?;
        drained.extend(active.iter().copied().filter(|member| !owned.contains(member)));
        let drain = if drained.is_empty() {
            None
        } else {
            Some(Arc::new(drain::Drain::open(
                &dir,
                labels,
                &drained,
                shown.retention,
                capacity,
            )?))
        };
        let mut outputs = Vec::new();
        let mut producers = Vec::new();
        let mut endpoints = Vec::new();
        let mut slots = Vec::new();
        let mut states = Vec::new();
        outputs.try_reserve_exact(count).map_err(io::Error::other)?;
        producers.try_reserve_exact(count).map_err(io::Error::other)?;
        endpoints
            .try_reserve_exact(active_count)
            .map_err(io::Error::other)?;
        slots.try_reserve_exact(count).map_err(io::Error::other)?;
        states.try_reserve_exact(count).map_err(io::Error::other)?;
        states.resize(count, StreamState::New);
        slots.resize_with(count, || CreditSlot::Inactive);
        outputs.resize_with(count, || None);
        producers.resize_with(count, || None);
        for &member in &drained {
            for channel in [Channel::Stdout, Channel::Stderr] {
                *slots
                    .get_mut(route_index(member, channel)?)
                    .ok_or_else(|| io::Error::other("drained credit index"))? = CreditSlot::Waiting;
            }
        }
        let credits = Arc::new(Credits {
            state: Mutex::new(Queue {
                slots,
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        });
        for (scope, address, endpoint, producer, control) in staged {
            let index = route_index(scope.member, scope.channel)?;
            *lock(&credits.state)
                .slots
                .get_mut(index)
                .ok_or_else(|| io::Error::other("active credit index"))? = CreditSlot::Waiting;
            let producer = Arc::new(producer);
            let weak_credits = Arc::downgrade(&credits);
            let quantum = capture_quantum(capacity)?;
            let output = Arc::new(Output {
                scope,
                address,
                control,
                metadata: Arc::clone(&metadata),
                life: Mutex::new(Life::Serving),
                credits: Arc::downgrade(&credits),
                index,
                learning: Mutex::new(Learning {
                    quantum,
                    sample: None,
                    admitted: capacity,
                }),
                charge: Arc::clone(&charge),
            });
            producer.on_credit(move || {
                if let Some(credits) = weak_credits.upgrade() {
                    credits.wake();
                }
            });
            *outputs
                .get_mut(index)
                .ok_or_else(|| io::Error::other("active output index"))? = Some(Arc::clone(&output));
            *producers
                .get_mut(index)
                .ok_or_else(|| io::Error::other("active producer index"))? = Some(producer);
            if let Some(endpoint) = endpoint {
                endpoints.push((endpoint, output));
            }
        }
        let reading = stream.try_clone()?;
        let writing = stream.try_clone()?;
        let mut input_slots: Vec<Option<Arc<Input>>> = Vec::new();
        input_slots
            .try_reserve_exact(labels.len())
            .map_err(io::Error::other)?;
        input_slots.resize_with(labels.len(), || None);
        let mut input_workers = Vec::new();
        input_workers
            .try_reserve_exact(staged_inputs.len())
            .map_err(io::Error::other)?;
        for (scope, address, endpoint, relay, control) in staged_inputs {
            let input = Arc::new(Input {
                scope,
                address,
                control,
                guest: Mutex::new(Guest::Awaited),
                arrived: Condvar::new(),
                metadata: Arc::clone(&metadata),
                recorded: std::sync::atomic::AtomicBool::new(false),
            });
            // An owner's abort, or the input let go, cannot wait for a record the member
            // does not read: its connection is ended under the relay.
            let closing = Arc::downgrade(&input);
            input.control.on_close(move || {
                if let Some(input) = closing.upgrade() {
                    input.close();
                }
            });
            *input_slots
                .get_mut(usize::try_from(scope.member).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("input member index"))? = Some(Arc::clone(&input));
            input_workers.push((endpoint, relay, input));
        }
        let (agent, agent_endpoint, agent_execs, agent_members) = match agent {
            Some((endpoint, revoke, execs, members)) => (
                Some(Agent {
                    address: endpoint.address,
                    revoke,
                }),
                Some(endpoint),
                execs,
                members,
            ),
            None => (None, None, None, None),
        };
        // Each exec the run's owner asks for is charged to the run's budget as it starts, as is
        // each lane to a member itself while it is open.
        let agent_execs = agent_execs.map(|(starts, admissions)| (starts, admissions, Arc::clone(&charge)));
        let ends = Arc::new(members::Ends::new(labels.len())?);
        let agent_members = agent_members.map(|lanes| (lanes, Arc::clone(&charge), Arc::clone(&ends)));
        let owner = Arc::new(Owner {
            outputs,
            inputs: input_slots,
            drain: drain.clone(),
            agent,
            stream,
            credits,
            charge,
            ends,
            told: Mutex::new(Told::Capturing),
        });
        let labels = copy_labels(labels)?;
        let producers = Arc::new(producers);
        {
            let mut runs = lock(&self.managed.runs);
            let entry = runs
                .get_mut(id)
                .filter(|e| matches!(e, Entry::Preparing))
                .ok_or_else(|| io::Error::other("managed run was cancelled"))?;
            *entry = Entry::Serving(Arc::clone(&owner));
        }
        for (endpoint, output) in endpoints {
            std::thread::Builder::new()
                .name("managed-output".into())
                .spawn_scoped(threads, move || {
                    if let Err(e) = endpoint.run() {
                        log(format!("managed output endpoint: {e}"));
                    }
                    output.abort();
                })?;
        }
        for (endpoint, relay, input) in input_workers {
            let relaying = Arc::clone(&input);
            std::thread::Builder::new()
                .name("managed-stdin".into())
                .spawn_scoped(threads, move || relay_input(&relaying, &relay))?;
            let Some(endpoint) = endpoint else {
                continue;
            };
            std::thread::Builder::new()
                .name("managed-input".into())
                .spawn_scoped(threads, move || {
                    if let Err(e) = endpoint.run() {
                        log(format!("managed input endpoint: {e}"));
                    }
                    input.control.abort();
                    input.close();
                })?;
        }
        if let Some(endpoint) = agent_endpoint {
            // What it serves, ended with it as each stream's own endpoint ends its stream.
            let mut agent_outputs = Vec::new();
            agent_outputs
                .try_reserve_exact(owner.outputs.len())
                .map_err(io::Error::other)?;
            agent_outputs.extend(owner.outputs.iter().flatten().map(Arc::clone));
            let mut agent_inputs = Vec::new();
            agent_inputs
                .try_reserve_exact(owner.inputs.len())
                .map_err(io::Error::other)?;
            agent_inputs.extend(owner.inputs.iter().flatten().map(Arc::clone));
            std::thread::Builder::new()
                .name("agent-control".into())
                .spawn_scoped(threads, move || {
                    if let Err(e) = endpoint.run() {
                        log(format!("agent-control endpoint: {e}"));
                    }
                    for output in &agent_outputs {
                        output.abort();
                    }
                    for input in &agent_inputs {
                        input.control.abort();
                        input.close();
                    }
                })?;
        }
        // A client attached to the run is shown what is drained as it is kept: a thread that
        // writes to it alone, which nothing waits on (drain.rs).
        // Its log lanes (D238) read what the drain keeps, and wait on its members' ends.
        let logged = drain.clone();
        if let (Some(drain), Some(mut to), Some(_)) = (drain, shown.to, shown.retention) {
            std::thread::Builder::new()
                .name("managed-shown".into())
                .spawn_scoped(threads, move || drain.show(&mut to))?;
        }
        // Its execs (D238), hosted until the endpoint that brings them ends.
        if let Some((starts, admissions, exec_charge)) = agent_execs {
            let run_id = id.to_owned();
            let mut running = Vec::new();
            running
                .try_reserve_exact(active.len())
                .map_err(io::Error::other)?;
            running.extend_from_slice(active);
            let run_dir = dir.clone();
            std::thread::Builder::new()
                .name("agent-execs".into())
                .spawn_scoped(threads, move || {
                    execs::host(
                        &starts,
                        admissions,
                        &exec_charge,
                        &|spec, argv, flags, stdio, theirs| {
                            self.launch_exec(&run_id, spec, argv, flags, stdio, theirs)
                        },
                        &log_lanes::Logs {
                            dir: &run_dir,
                            active: &running,
                            drain: logged.as_deref(),
                            run_end: &|| self.wake_at_end(&run_id),
                            forget: &|waiter| self.forget_waiter(&run_id, waiter),
                        },
                    );
                })?;
        }
        // Its members' own lanes (D238), served until the endpoint that brings them ends.
        if let Some((lanes, member_charge, member_ends)) = agent_members {
            let run_id = id.to_owned();
            let mut sized = Vec::new();
            sized
                .try_reserve_exact(terminals.len())
                .map_err(io::Error::other)?;
            sized.extend_from_slice(terminals);
            std::thread::Builder::new()
                .name("agent-members".into())
                .spawn_scoped(threads, move || {
                    members::host(
                        &lanes,
                        &members::Hosting {
                            ends: &member_ends,
                            terminals: &sized,
                            charge: &member_charge,
                            started: &|| {
                                self.await_start(&run_id);
                                self.running(&run_id)
                            },
                            builtin: &|spec, stdio, theirs| self.launch_builtin(&run_id, spec, stdio, theirs),
                        },
                    );
                })?;
        }
        let writer_owner = Arc::clone(&owner);
        let writer_producers = Arc::clone(&producers);
        let (credited, captured) = (id.to_owned(), id.to_owned());
        std::thread::Builder::new()
            .name("managed-credit".into())
            .spawn_scoped(threads, move || {
                let (failed, ended) = run_credits(writing, &writer_owner, &writer_producers);
                if let Some(e) = failed {
                    self.capture_failed(&credited, "its members' output's credits", &e);
                }
                if let Some(ended) = ended {
                    self.end_after_relays(&credited, ended);
                }
            })?;
        let reader_owner = Arc::clone(&owner);
        std::thread::Builder::new()
            .name("managed-capture".into())
            .spawn_scoped(threads, move || {
                if let Some(e) = run_bridge(reading, &reader_owner, &labels, states) {
                    self.capture_failed(&captured, "its members' output", &e);
                }
            })?;
        setup.try_reserve(3).map_err(io::Error::other)?;
        setup.push(format!("managed-capacity={}", run::MAX_PAYLOAD).into_bytes());
        setup.push(format!("managed-members={members}").into_bytes());
        // Each member's end told as it comes, for its own lanes (D238); and each member given a
        // terminal of its own (D244).
        setup
            .try_reserve(usize::from(agent_control).saturating_add(terminals.len()))
            .map_err(io::Error::other)?;
        if agent_control {
            setup.push(run::members::ENDS.to_vec());
        }
        setup.extend(terminals.iter().map(|&member| run::members::terminal(member)));
        let mut activation = Vec::new();
        activation
            .try_reserve_exact(
                run::managed::ACTIVE_KEY
                    .len()
                    .checked_add(
                        active
                            .len()
                            .checked_mul(size_of::<u32>())
                            .ok_or_else(|| io::Error::other("active mask overflow"))?,
                    )
                    .ok_or_else(|| io::Error::other("active mask overflow"))?,
            )
            .map_err(io::Error::other)?;
        activation.extend_from_slice(run::managed::ACTIVE_KEY);
        for member in active {
            activation.extend_from_slice(&member.to_be_bytes());
        }
        setup.push(activation);
        if !inputs.is_empty() {
            let mut named = Vec::new();
            named
                .try_reserve_exact(
                    run::managed::INPUT_KEY
                        .len()
                        .checked_add(
                            inputs
                                .len()
                                .checked_mul(size_of::<u32>())
                                .ok_or_else(|| io::Error::other("input mask overflow"))?,
                        )
                        .ok_or_else(|| io::Error::other("input mask overflow"))?,
                )
                .map_err(io::Error::other)?;
            named.extend_from_slice(run::managed::INPUT_KEY);
            for member in inputs {
                named.extend_from_slice(&member.to_be_bytes());
            }
            setup.try_reserve(1).map_err(io::Error::other)?;
            setup.push(named);
        }
        // The members it drains, whose pipes init reads to their ends as the run ends.
        if let Some(drain) = &owner.drain {
            setup.try_reserve(1).map_err(io::Error::other)?;
            setup.push(drained_entry(drain.members())?);
        }
        if owner
            .outputs
            .iter()
            .flatten()
            .any(|output| !matches!(*lock(&output.life), Life::Serving))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed output ended before actor release",
            )
            .into());
        }
        Ok(peer)
    }

    /// Runs one of init's built-ins in run `id`'s microVM for one of its members' own lanes
    /// (D238): once the run has started, not while it is paused, handed to its VM process as the
    /// daemon's own built-ins are, told (`EXEC_TOLD`) and no client's exec.
    fn launch_builtin(
        &self,
        id: &str,
        spec: &run::Spec,
        stdio: [shards_ipc::fd::BorrowedFd<'_>; 3],
        theirs: &Stream,
    ) -> io::Result<()> {
        self.await_start(id);
        if lock(&self.paused).contains(id) {
            return Err(io::Error::other("its microVM is paused"));
        }
        self.hand_exec(id, spec, None, shards_ipc::EXEC_TOLD, stdio, theirs)
    }

    /// Starts an agent-control exec in run `id`'s microVM (D238): once the run has started,
    /// as an ask may come while it starts; not while it is paused, as `docker exec` refuses
    /// one; handed to its VM process as `shards exec --for` hands one (D243).
    fn launch_exec(
        &self,
        id: &str,
        spec: &run::Spec,
        argv: &[String],
        flags: u8,
        stdio: [shards_ipc::fd::BorrowedFd<'_>; 3],
        theirs: &Stream,
    ) -> io::Result<()> {
        self.await_start(id);
        if lock(&self.paused).contains(id) {
            return Err(io::Error::other("its microVM is paused"));
        }
        self.hand_exec(id, spec, Some(argv), flags, stdio, theirs)
    }
}

/// The setup entry naming the members the daemon drains (`run::managed::DRAINED_KEY`): their
/// indices, ascending, in decimal.
fn drained_entry(members: &[u32]) -> io::Result<Vec<u8>> {
    let mut entry = Vec::new();
    // Each index's decimal digits at their widest, and its comma.
    let most = members
        .len()
        .checked_mul(u32::MAX.ilog10() as usize + 2)
        .and_then(|n| n.checked_add(run::managed::DRAINED_KEY.len()))
        .ok_or_else(|| io::Error::other("drained member list overflow"))?;
    entry.try_reserve_exact(most).map_err(io::Error::other)?;
    entry.extend_from_slice(run::managed::DRAINED_KEY);
    for (n, member) in members.iter().enumerate() {
        if n > 0 {
            entry.push(b',');
        }
        write!(entry, "{member}")?;
    }
    Ok(entry)
}

fn copy_labels(labels: &[String]) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    out.try_reserve_exact(labels.len()).map_err(io::Error::other)?;
    for label in labels {
        let mut copy = String::new();
        copy.try_reserve_exact(label.len()).map_err(io::Error::other)?;
        copy.push_str(label);
        out.push(copy);
    }
    Ok(out)
}

/// A capture's credit worker, as the daemon runs it ([`send_credits`]): every keep is its, so
/// once it has ended its drain's logs are whole (D240), and its viewer shows them to their
/// end. How it failed, for its daemon to judge (`capture_failed`, D258), and the run's end its
/// capture held, to be told now.
fn run_credits(
    stream: Stream,
    owner: &Owner,
    producers: &[Option<Arc<Producer>>],
) -> (Option<Failure>, Option<super::Ended>) {
    let failed = send_credits(stream, owner, producers).err();
    if failed.is_some() {
        owner.abort();
    }
    if let Some(drain) = &owner.drain {
        drain.close();
    }
    (failed, owner.captured())
}

/// A capture's bridge, as the daemon runs it ([`bridge`]): where its stream ends before its
/// channels do, or fails, nothing more is read of it, and its capture is cut ([`Owner::cut`]).
/// How it failed, for its daemon to judge (`capture_failed`, D258): its VM's end of the stream,
/// as its run ends, among them.
fn run_bridge(stream: Stream, owner: &Owner, labels: &[String], states: Vec<StreamState>) -> Option<Failure> {
    let failed = bridge(stream, owner, labels, states).err();
    if failed.is_some() {
        owner.cut();
    }
    failed
}

fn send_credits(
    mut stream: Stream,
    owner: &Owner,
    producers: &[Option<Arc<Producer>>],
) -> Result<(), Failure> {
    let mut drained = drain::Windows::new(owner.drain.as_deref())?;
    while let Some(action) = owner.credits.next(|index| {
        // The daemon takes a drained channel's records whenever they come: it waits on no
        // reader (drain.rs).
        if drained.covers(index) {
            return Ok(State::Ready(Progress {
                accepted: 0,
                consumed: 0,
            }));
        }
        owner
            .outputs
            .get(index)
            .and_then(Option::as_ref)
            .map(|output| output.control.state())
            .ok_or_else(|| io::Error::other("unknown output channel"))
    })? {
        match action {
            Action::Grant(index) if drained.covers(index) => {
                let (capacity, bytes) = drained.grant(index, &owner.charge)?;
                {
                    let mut state = lock(&owner.credits.state);
                    let Some(slot @ CreditSlot::Sending) = state.slots.get_mut(index) else {
                        drained.lend(index, bytes);
                        continue;
                    };
                    *slot = CreditSlot::Granted {
                        capacity,
                        began: Instant::now(),
                        bytes,
                    };
                }
                let (member, channel) = route_of(index)?;
                credit_or_end(&mut stream, member, channel, capacity);
            }
            Action::Submit(index, bytes, after) if drained.covers(index) => {
                let drain = owner
                    .drain
                    .as_ref()
                    .ok_or_else(|| io::Error::other("a drained channel without its drain"))?;
                let ended = drain.keep(&bytes)? || matches!(after, After::Terminal(_));
                drained.kept(index, bytes);
                let mut state = lock(&owner.credits.state);
                if let Some(slot @ CreditSlot::Sending) = state.slots.get_mut(index) {
                    *slot = if ended {
                        CreditSlot::Ended
                    } else {
                        CreditSlot::Waiting
                    };
                }
            }
            Action::Grant(index) => {
                let output = owner
                    .outputs
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| io::Error::other("unknown output channel"))?;
                let producer = producers
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| io::Error::other("unknown producer"))?;
                output.observed();
                let capacity = output.capacity(producer)?;
                let Some(bytes) = producer.take_buffer().map_err(Failure::Endpoint)? else {
                    let mut state = lock(&owner.credits.state);
                    if let Some(slot @ CreditSlot::Sending) = state.slots.get_mut(index) {
                        *slot = CreditSlot::Waiting;
                    }
                    continue;
                };
                {
                    let mut state = lock(&owner.credits.state);
                    let Some(slot @ CreditSlot::Sending) = state.slots.get_mut(index) else {
                        continue;
                    };
                    *slot = CreditSlot::Granted {
                        capacity,
                        began: Instant::now(),
                        bytes,
                    };
                }
                credit_or_end(&mut stream, output.scope.member, output.scope.channel, capacity);
            }
            Action::Submit(index, bytes, after) => {
                if let Some(output) = owner.outputs.get(index).and_then(Option::as_ref) {
                    output.observed();
                }
                let producer = producers
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| io::Error::other("unknown capture producer"))?;
                let result = producer.submit(bytes);
                let mut state = lock(&owner.credits.state);
                let slot = state
                    .slots
                    .get_mut(index)
                    .ok_or_else(|| io::Error::other("unknown capture credit"))?;
                match result {
                    Ok(()) if matches!(after, After::Terminal(_)) => {
                        // A concurrent revocation must not reopen this credit slot.
                        if !matches!(slot, CreditSlot::Sending) {
                            continue;
                        }
                        *slot = CreditSlot::Draining;
                        drop(state);
                        if let After::Terminal(terminal) = after
                            && let Some(output) = owner.outputs.get(index).and_then(Option::as_ref)
                            && let Err(error) = output.finish(terminal)
                        {
                            log(format!("managed output finish: {error}"));
                            output.abort();
                        }
                    }
                    Ok(()) => {
                        if matches!(slot, CreditSlot::Sending) {
                            *slot = CreditSlot::Waiting;
                        }
                    }
                    Err(Rejected::Full(bytes)) => {
                        if matches!(slot, CreditSlot::Sending) {
                            *slot = CreditSlot::Captured(bytes, after);
                        }
                    }
                    Err(Rejected::Closed(_) | Rejected::Invalid(_)) => {
                        *slot = CreditSlot::Ended;
                        drop(state);
                        if let Some(output) = owner.outputs.get(index).and_then(Option::as_ref) {
                            output.abort();
                        }
                    }
                }
            }
            Action::Complete(index) => {
                if let Some(output) = owner.outputs.get(index).and_then(Option::as_ref) {
                    output.complete();
                }
            }
        }
    }
    Ok(())
}

/// [`send_credit`], where a credit its VM can no longer take ends what the daemon sends it, and
/// nothing more: what it sent before it went is still read to its end by the capture's bridge,
/// which then ends the capture ([`Owner::cut`]). Its reads are not ended here: on macOS a
/// socket's read side shut drops what it holds unread (XNU bsd/kern/uipc_socket.c,
/// `soshutdownlock_final`'s `sorflush`).
fn credit_or_end(stream: &mut Stream, member: u32, channel: Channel, capacity: usize) {
    if send_credit(stream, member, channel, capacity).is_err() {
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
}

/// Credits `capacity` bytes of member `member`'s `channel` to its VM.
fn send_credit(stream: &mut Stream, member: u32, channel: Channel, capacity: usize) -> io::Result<()> {
    let credit = run::managed::Credit {
        member,
        channel,
        capacity: u64::try_from(capacity).map_err(io::Error::other)?,
    }
    .encode()
    .map_err(|e| io::Error::other(e.to_string()))?;
    let len = u32::try_from(credit.len()).map_err(io::Error::other)?;
    stream.write_all(&run::header(run::kind::MANAGED_CREDIT, len))?;
    stream.write_all(&credit)
}

fn bridge(
    stream: Stream,
    owner: &Owner,
    labels: &[String],
    mut states: Vec<StreamState>,
) -> Result<(), Failure> {
    loop {
        let mut head = [0; run::HEADER];
        read_exact(&stream, &mut head)?;
        let (_, len) = run::parse_header(head)
            .filter(|(kind, len)| *kind == run::kind::MANAGED && *len as usize >= run::managed::HEADER)
            .ok_or_else(|| io::Error::other("invalid capture record header"))?;
        let mut prefix = [0; run::managed::HEADER];
        read_exact(&stream, &mut prefix)?;
        let (member, channel) =
            run::managed::route_prefix(&prefix).map_err(|e| io::Error::other(e.to_string()))?;
        let route = route_index(member, channel)?;
        let (capacity, began, mut bytes) = owner.credits.begin_record(route, len as usize)?;
        if bytes.capacity() < len as usize {
            return Err(io::Error::other("capture buffer is smaller than issued credit").into());
        }
        bytes.resize(len as usize, 0);
        bytes
            .get_mut(..run::managed::HEADER)
            .ok_or_else(|| io::Error::other("short capture record"))?
            .copy_from_slice(&prefix);
        read_exact(
            &stream,
            bytes
                .get_mut(run::managed::HEADER..)
                .ok_or_else(|| io::Error::other("short capture record"))?,
        )?;
        let index = accept(&bytes, labels, &mut states)?;
        if let Some(output) = owner.outputs.get(index).and_then(Option::as_ref)
            && let Event::Data(data) = Record::decode(&bytes)
                .map_err(|e| io::Error::other(e.to_string()))?
                .event
            && bytes.len() == capacity
        {
            let end = output
                .control
                .progress()
                .accepted
                .checked_add(u64::try_from(bytes.len()).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("capture sample offset overflow"))?;
            lock(&output.learning).sample = Some(Sample {
                began,
                end,
                bytes: data.len() as u64,
            });
        }
        let after = match states.get(index) {
            Some(StreamState::Ended { .. }) => After::Terminal(Terminal::End),
            Some(StreamState::Aborted { errno, .. }) => After::Terminal(Terminal::Aborted(*errno)),
            _ => After::More,
        };
        owner.credits.captured(index, bytes, after)?;
        if states
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                owner.outputs.get(*index).is_some_and(Option::is_some)
                    || owner.drain.as_ref().is_some_and(|drain| drain.drains(*index))
            })
            .all(|(_, s)| s.terminal())
        {
            return Ok(());
        }
    }
}

/// The member and channel of credit route `index` ([`route_index`]).
fn route_of(index: usize) -> io::Result<(u32, Channel)> {
    let member = u32::try_from(index / 2).map_err(io::Error::other)?;
    let channel = if index.is_multiple_of(2) {
        Channel::Stdout
    } else {
        Channel::Stderr
    };
    Ok((member, channel))
}

fn route_index(member: u32, channel: Channel) -> io::Result<usize> {
    let channel = match channel {
        Channel::Stdout => 0,
        Channel::Stderr => 1,
    };
    usize::try_from(member)
        .ok()
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(channel))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "capture state index overflow"))
}

fn accept(bytes: &[u8], labels: &[String], states: &mut [StreamState]) -> io::Result<usize> {
    let record =
        Record::decode(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let member = usize::try_from(record.member).map_err(io::Error::other)?;
    let label = labels
        .get(member)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown capture member"))?;
    if matches!(record.event, Event::Open(said) if said != label) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "capture label differs from trusted image",
        ));
    }
    let index = route_index(record.member, record.channel)?;
    states
        .get_mut(index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "capture state was not admitted"))?
        .accept(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(index)
}

fn owner_key(home: &Path, client: &str, local: Fingerprint) -> Result<Fingerprint, Failure> {
    if client.is_empty() {
        return Ok(local);
    }
    let enrolled = crate::context::clients(home)
        .into_iter()
        .find(|c| c.name == client)
        .ok_or(Failure::Auth(Auth::UnknownContext))?;
    if !identity::is_ed25519(&enrolled.key) {
        return Err(Failure::Auth(Auth::InvalidKey));
    }
    Ok(identity::fingerprint(&enrolled.key))
}

fn record_capacity(labels: &[String], active: &[u32]) -> io::Result<usize> {
    active
        .iter()
        .map(|member| {
            labels
                .get(*member as usize)
                .ok_or_else(|| io::Error::other("unknown trusted managed label"))
        })
        .map(|label| {
            let label = label?;
            Record {
                member: 0,
                channel: Channel::Stdout,
                offset: 0,
                event: Event::Open(label),
            }
            .encoded_len()
            .map_err(|e| io::Error::other(e.to_string()))
        })
        .try_fold(run::managed::HEADER + size_of::<i32>(), |most, n| {
            n.map(|n| most.max(n))
        })
}

/// The run's metadata record, which its channels append their terminal records to: whole
/// lines only, each a record an owner reads alone. A channel's record is formatted whole
/// and written at once; one whose write fails part-way is cut back to the last whole line
/// before another channel appends.
#[derive(Debug)]
struct Ledger {
    file: File,
    whole: Whole,
}

/// How far the run's metadata record is whole lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Whole {
    /// Every byte before this offset, where the next record starts.
    To(u64),
    /// A record's write failed part-way and could not be cut back: no line may follow its
    /// part, so no record is appended.
    Torn,
}

impl Ledger {
    fn new(mut file: File) -> io::Result<Self> {
        let at = file.stream_position()?;
        Ok(Self {
            file,
            whole: Whole::To(at),
        })
    }

    fn append(&mut self, record: &[u8]) -> io::Result<()> {
        let Whole::To(at) = self.whole else {
            return Err(io::Error::other("the managed output record ends in a torn line"));
        };
        let end = u64::try_from(record.len())
            .ok()
            .and_then(|len| at.checked_add(len))
            .ok_or_else(|| io::Error::other("managed output record length overflow"))?;
        if let Err(error) = self.file.write_all(record) {
            let cut = self
                .file
                .set_len(at)
                .and_then(|()| self.file.seek(io::SeekFrom::Start(at)));
            if cut.is_err() {
                self.whole = Whole::Torn;
            }
            return Err(error);
        }
        self.whole = Whole::To(end);
        Ok(())
    }
}

fn shared_metadata(file: File) -> io::Result<Arc<Mutex<Ledger>>> {
    Ledger::new(file).map(|ledger| Arc::new(Mutex::new(ledger)))
}

/// The byte quantum a channel's credit climbs in (D226), from the run's first record. A
/// record crosses two frames, each bounded by its protocol: the run's from the guest
/// (`run::MAX_PAYLOAD`) and the endpoint's data frame to the owner (`RECORD_MOST`, its
/// frame less the record's offset). It climbs to the smaller, so that every window it
/// desires is one the endpoint can admit.
fn capture_quantum(capacity: usize) -> io::Result<shards_registry::Quantum> {
    let most = usize::try_from(run::MAX_PAYLOAD)
        .map_err(io::Error::other)?
        .min(shards_remote::managed::RECORD_MOST);
    shards_registry::Quantum::new(capacity, most)
        .ok_or_else(|| io::Error::other("invalid capture quantum bounds"))
}

fn private_record(path: &Path) -> io::Result<File> {
    match shards_ipc::create_private(path) {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().write(true).truncate(true).open(path)
        }
        Err(e) => Err(e),
    }
}

fn generation(dir: &Path) -> io::Result<u64> {
    let path = dir.join(GENERATION);
    let mut file = match shards_ipc::create_private(&path) {
        Ok(mut file) => {
            file.write_all(&1u64.to_be_bytes())?;
            shards_vmm::platform::sync_durable(&file)?;
            shards_vmm::platform::sync_dir(dir)?;
            return Ok(1);
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().read(true).write(true).open(path)?
        }
        Err(e) => return Err(e),
    };
    file.lock()?;
    let result = (|| {
        if file.metadata()?.len() != size_of::<u64>() as u64 {
            return Err(io::Error::other("invalid managed output generation record"));
        }
        let mut bytes = [0; size_of::<u64>()];
        file.read_exact(&mut bytes)?;
        let next = u64::from_be_bytes(bytes)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("managed output generation exhausted"))?;
        file.rewind()?;
        file.write_all(&next.to_be_bytes())?;
        shards_vmm::platform::sync_durable(&file)?;
        Ok(next)
    })();
    let unlocked = file.unlock();
    result.and_then(|value| unlocked.map(|()| value))
}

fn read_exact(stream: &Stream, mut bytes: &mut [u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = shards_ipc::read(stream, bytes)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes = bytes
            .get_mut(count..)
            .ok_or_else(|| io::Error::other("capture read overflow"))?;
    }
    Ok(())
}

fn ipc_buffers(stream: &Stream) -> io::Result<u64> {
    #[cfg(unix)]
    {
        [libc::SO_SNDBUF, libc::SO_RCVBUF]
            .into_iter()
            .try_fold(0u64, |sum, option| {
                let mut bytes: libc::c_int = 0;
                let mut len = size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: this owned private socket and an integer output of its size.
                let rc = unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        option,
                        (&raw mut bytes).cast(),
                        &raw mut len,
                    )
                };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                if bytes <= 0 || len as usize != size_of::<libc::c_int>() {
                    return Err(io::Error::other("unknown capture socket queue"));
                }
                sum.checked_add(bytes as u64)
                    .ok_or_else(|| io::Error::other("capture socket queue overflow"))
            })
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Pipes::GetNamedPipeInfo;
        let (mut input, mut output) = (0u32, 0u32);
        // SAFETY: an owned pipe and correctly sized output pointers, no flags requested.
        let rc = unsafe {
            GetNamedPipeInfo(
                stream.as_raw_fd(),
                std::ptr::null_mut(),
                &raw mut output,
                &raw mut input,
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            return Err(io::Error::last_os_error());
        }
        if input == 0 || output == 0 {
            return Err(io::Error::other("capture pipe queues grow without a known bound"));
        }
        Ok(u64::from(input) + u64::from(output))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod terminal_tests {
    use super::*;

    fn at(accepted: u64, consumed: u64) -> Progress {
        Progress { accepted, consumed }
    }

    fn scope() -> Scope {
        Scope {
            run: RunId([7; 32]),
            generation: 1,
            member: 1,
            channel: Channel::Stdout,
        }
    }

    fn queue(slots: Vec<CreditSlot>) -> Credits {
        Credits {
            state: Mutex::new(Queue {
                slots,
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        }
    }

    #[test]
    fn issued_capture_storage_survives_revocation_until_its_record_is_drained() {
        let bytes = Vec::with_capacity(16);
        let pointer = bytes.as_ptr();
        let credits = queue(vec![
            CreditSlot::Granted {
                capacity: 16,
                began: Instant::now(),
                bytes,
            },
            CreditSlot::Waiting,
        ]);
        assert!(credits.begin_record(0, 17).is_err());
        credits.revoke(0);
        let (capacity, _, mut bytes) = credits.begin_record(0, 8).unwrap();
        assert_eq!(capacity, 16);
        assert_eq!(bytes.as_ptr(), pointer);
        bytes.resize(8, 0);
        assert_eq!(bytes.as_ptr(), pointer);
        credits.captured(0, bytes, After::More).unwrap();
        assert!(matches!(
            lock(&credits.state).slots.first(),
            Some(CreditSlot::Revoked)
        ));
        assert!(matches!(
            credits.next(|_| Ok(State::Ready(at(0, 0)))).unwrap(),
            Some(Action::Grant(1))
        ));
    }

    #[test]
    fn captured_storage_submits_while_its_endpoint_is_the_explicit_borrower() {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(b"record");
        let pointer = bytes.as_ptr();
        let credits = queue(vec![CreditSlot::Captured(bytes, After::More)]);
        match credits.next(|_| Ok(State::Capturing(at(0, 0)))).unwrap() {
            Some(Action::Submit(0, bytes, After::More)) => {
                assert_eq!(bytes.as_ptr(), pointer);
                assert_eq!(bytes, b"record");
            }
            _ => panic!("the capture loan was not submitted"),
        }
        credits.close();
        assert!(
            credits
                .next(|_| panic!("closed owner must not probe"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn prepared_growth_rolls_back_or_commits_the_replacement_charge() {
        let budget = Arc::new(Mutex::new(Some(80)));
        let charge = Charge {
            budget: Arc::clone(&budget),
            bytes: Mutex::new(80),
        };
        {
            let _pending = charge.prepare_growth(40).unwrap();
            assert_eq!(*lock(&budget), Some(120));
        }
        assert_eq!(*lock(&budget), Some(80));
        charge.prepare_growth(40).unwrap().commit(30).unwrap();
        assert_eq!(*lock(&budget), Some(90));
        assert_eq!(*lock(&charge.bytes), 90);
        drop(charge);
        assert_eq!(*lock(&budget), Some(0));
    }

    #[test]
    fn known_normal_completion_survives_stop_before_the_worker_publishes() {
        for wire in [99, 102] {
            let mut life = Life::Serving;
            life.drain(Terminal::End).unwrap();
            let remote = State::Finished(at(wire, wire));
            // The actual r668 ordering: End/ACK/EOF, delayed publication, then stop.
            assert_eq!(life.resolve(remote, Resolve::Stop), Some(Life::Ended));
            let mut buffer = [0; TERMINAL_MOST];
            assert_eq!(
                life.record(scope(), remote.progress(), &mut buffer).unwrap(),
                format!("member 1 channel 1 state ended accepted {wire} consumed {wire} tail complete\n")
                    .as_bytes()
            );
            assert_eq!(life.resolve(State::Aborted(at(wire, wire)), Resolve::Stop), None);
            assert_eq!(life.resolve(remote, Resolve::Complete), None);
            assert!(life.drain(Terminal::End).is_err());
            assert_eq!(life, Life::Ended);
        }
    }

    #[test]
    fn pending_or_aborted_equal_cursors_cannot_publish_normal_completion() {
        for state in [
            State::Pending(at(102, 102)),
            State::Ready(at(102, 102)),
            State::Aborted(at(102, 102)),
        ] {
            let mut life = Life::Draining(Terminal::End);
            let completed = life.resolve(state, Resolve::Complete);
            assert_ne!(completed, Some(Life::Ended));
            life.resolve(state, Resolve::Stop);
            assert_eq!(life, Life::Aborted(Some(Terminal::End)));
            assert_eq!(
                life.resolve(State::Finished(at(102, 102)), Resolve::Complete),
                None
            );
        }
    }

    #[test]
    fn stop_before_ack_is_sticky_and_retains_the_known_canonical_failure() {
        for terminal in [Terminal::End, Terminal::Aborted(5), Terminal::Aborted(i32::MAX)] {
            let mut life = Life::Draining(terminal);
            assert_eq!(life.resolve(State::Pending(at(102, 86)), Resolve::Complete), None);
            assert_eq!(
                life.resolve(State::Pending(at(102, 86)), Resolve::Stop),
                Some(Life::Aborted(Some(terminal)))
            );
            assert_eq!(
                life.resolve(State::Finished(at(102, 102)), Resolve::Complete),
                None
            );
            assert_eq!(life.resolve(State::Finished(at(102, 102)), Resolve::Stop), None);
            assert!(life.drain(terminal).is_err());
        }
        let mut serving = Life::Serving;
        assert_eq!(
            serving.resolve(State::Ready(at(0, 0)), Resolve::Stop),
            Some(Life::Aborted(None))
        );
        assert!(serving.drain(Terminal::End).is_err());
    }

    #[test]
    fn fully_consumed_canonical_abort_keeps_errno_and_unknown_tail() {
        let mut life = Life::Draining(Terminal::Aborted(5));
        assert_eq!(
            life.resolve(State::Finished(at(20, 20)), Resolve::Complete),
            Some(Life::Aborted(Some(Terminal::Aborted(5))))
        );
        let mut buffer = [0; TERMINAL_MOST];
        assert_eq!(
            life.record(scope(), at(20, 20), &mut buffer).unwrap(),
            b"member 1 channel 1 canonical aborted errno 5\nmember 1 channel 1 state aborted accepted 20 consumed 20 tail unknown\n"
        );
        assert_eq!(life.resolve(State::Finished(at(20, 20)), Resolve::Stop), None);
    }

    #[test]
    fn metadata_failure_is_returned_without_changing_the_logical_winner() {
        let dir = shards_testdir::TempDir::new("managed-refuse").unwrap();
        File::create(dir.join("metadata")).unwrap();
        for life in [Life::Ended, Life::Aborted(Some(Terminal::Aborted(5)))] {
            // A record its file refuses: one opened for reading alone.
            let mut ledger = Ledger::new(File::open(dir.join("metadata")).unwrap()).unwrap();
            let mut buffer = [0; TERMINAL_MOST];
            let record = life.record(scope(), at(20, 20), &mut buffer).unwrap();
            assert!(ledger.append(record).is_err());
            let mut sticky = life;
            assert_eq!(sticky.resolve(State::Aborted(at(20, 20)), Resolve::Stop), None);
            assert_eq!(sticky, life);
        }
    }

    /// A channel's record at its widest, every number at its type's most, fits the record's
    /// buffer.
    #[test]
    fn the_widest_terminal_record_fits_its_buffer() {
        let scope = Scope {
            member: u32::MAX,
            channel: Channel::Stderr,
            ..scope()
        };
        let mut buffer = [0; TERMINAL_MOST];
        let record = Life::Aborted(Some(Terminal::Aborted(i32::MIN)))
            .record(scope, at(u64::MAX, u64::MAX), &mut buffer)
            .unwrap();
        assert_eq!(
            record,
            format!(
                "member {0} channel 2 canonical aborted errno {1}\n\
                 member {0} channel 2 state aborted accepted {2} consumed {2} tail unknown\n",
                u32::MAX,
                i32::MIN,
                u64::MAX
            )
            .as_bytes()
        );
        // The channel's one digit two short of a u8's three, on each of the two lines, and
        // `unknown` one short of `complete`.
        assert_eq!(record.len(), TERMINAL_MOST - 5);
        let mut buffer = [0; TERMINAL_MOST];
        assert!(
            Life::Serving
                .record(scope, at(1, 1), &mut buffer)
                .unwrap()
                .is_empty()
        );
    }

    /// A record whose failed write cannot be cut back takes no more lines: none may follow
    /// the part a failed write may have left.
    #[test]
    fn a_record_that_cannot_be_cut_back_takes_no_more_lines() {
        let dir = shards_testdir::TempDir::new("managed-torn-state").unwrap();
        File::create(dir.join("metadata")).unwrap();
        // Opened for reading alone: its writes and its cuts both fail.
        let mut ledger = Ledger::new(File::open(dir.join("metadata")).unwrap()).unwrap();
        assert_eq!(ledger.whole, Whole::To(0));
        assert!(ledger.append(b"member 1 channel 1 state ended\n").is_err());
        assert_eq!(ledger.whole, Whole::Torn);
        assert!(ledger.append(b"member 1 channel 2 state ended\n").is_err());
        let mut whole = Ledger::new(File::create(dir.join("whole")).unwrap()).unwrap();
        whole.append(b"a\n").unwrap();
        whole.append(b"bc\n").unwrap();
        assert_eq!(whole.whole, Whole::To(5));
        assert_eq!(std::fs::read(dir.join("whole")).unwrap(), b"a\nbc\n");
    }

    #[test]
    fn draining_completion_is_actionable_without_capacity_and_only_once() {
        let credits = queue(vec![CreditSlot::Draining]);
        let remote = State::Finished(at(102, 102));
        assert!(matches!(
            credits.next(|_| Ok(remote)).unwrap(),
            Some(Action::Complete(0))
        ));
        credits.wake();
        credits.wake();
        assert!(
            credits
                .next(|_| panic!("ended credit must not probe again"))
                .unwrap()
                .is_none()
        );
        let credits = queue(vec![CreditSlot::Draining]);
        assert!(matches!(
            credits.next(|_| Ok(State::Aborted(at(102, 86)))).unwrap(),
            Some(Action::Complete(0))
        ));
    }

    #[test]
    fn endpoint_loss_in_serving_or_drain_converges_without_waiting_for_capacity() {
        let remote = State::Aborted(at(102, 86));
        for initial in [
            Life::Serving,
            Life::Draining(Terminal::End),
            Life::Draining(Terminal::Aborted(5)),
        ] {
            let mut life = initial;
            let expected = match initial {
                Life::Draining(terminal) => Life::Aborted(Some(terminal)),
                _ => Life::Aborted(None),
            };
            assert_eq!(life.resolve(remote, Resolve::Complete), Some(expected));
            assert_eq!(life.resolve(remote, Resolve::Stop), None);
        }
        for slot in [
            CreditSlot::Waiting,
            CreditSlot::Captured(vec![1], After::More),
            CreditSlot::Draining,
        ] {
            let credits = queue(vec![slot]);
            assert!(matches!(
                credits.next(|_| Ok(remote)).unwrap(),
                Some(Action::Complete(0))
            ));
            credits.revoke(0);
            credits.wake();
            assert!(
                credits
                    .next(|_| panic!("lost endpoint credit must be retired"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn drain_wait_rechecks_the_predicate_across_the_atomic_condvar_release() {
        let credits = queue(vec![CreditSlot::Draining]);
        let remote = Mutex::new(State::Pending(at(102, 86)));
        let (inspected, observed) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|threads| {
            let worker = threads.spawn(|| {
                credits.next(|_| {
                    let state = *lock(&remote);
                    if matches!(state, State::Pending(_)) {
                        let _ = inspected.try_send(());
                    }
                    Ok(state)
                })
            });
            // The worker observed Pending while holding credit. Publication drops
            // its source lock before wake takes credit, including this wait seam.
            observed.recv().unwrap();
            *lock(&remote) = State::Finished(at(102, 102));
            credits.wake();
            assert!(matches!(
                worker.join().unwrap().unwrap(),
                Some(Action::Complete(0))
            ));
        });
    }

    #[test]
    fn revocation_and_close_wake_a_drain_without_reopening_it() {
        for close in [false, true] {
            let credits = queue(vec![CreditSlot::Draining]);
            let (inspected, observed) = std::sync::mpsc::sync_channel(1);
            std::thread::scope(|threads| {
                let worker = threads.spawn(|| {
                    credits.next(|_| {
                        let _ = inspected.try_send(());
                        Ok(State::Pending(at(102, 86)))
                    })
                });
                observed.recv().unwrap();
                if close {
                    credits.close();
                } else {
                    credits.revoke(0);
                }
                credits.wake();
                assert!(worker.join().unwrap().unwrap().is_none());
            });
        }
    }

    #[test]
    fn readiness_is_derived_and_a_spurious_wake_does_not_issue_credit() {
        let waiting = queue(vec![CreditSlot::Waiting]);
        let remote = Mutex::new(State::Pending(at(0, 0)));
        let (inspected, observed) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|threads| {
            let worker = threads.spawn(|| {
                waiting.next(|_| {
                    let state = *lock(&remote);
                    if matches!(state, State::Pending(_)) {
                        let _ = inspected.try_send(());
                    }
                    Ok(state)
                })
            });
            observed.recv().unwrap();
            waiting.wake();
            observed.recv().unwrap();
            *lock(&remote) = State::Ready(at(0, 0));
            waiting.wake();
            assert!(matches!(worker.join().unwrap().unwrap(), Some(Action::Grant(0))));
        });
        let credits = queue(vec![
            CreditSlot::Waiting,
            CreditSlot::Captured(vec![1], After::More),
        ]);
        credits.wake();
        let remote = State::Ready(at(0, 0));
        assert!(matches!(
            credits.next(|_| Ok(remote)).unwrap(),
            Some(Action::Grant(0))
        ));
        assert!(matches!(
            credits.next(|_| Ok(remote)).unwrap(),
            Some(Action::Submit(1, _, After::More))
        ));
        credits.revoke(0);
        credits.revoke(1);
        credits.wake();
        assert!(
            credits
                .next(|_| panic!("revoked credit must not probe"))
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn encoded(member: u32, channel: Channel, offset: u64, event: Event<'_>) -> Vec<u8> {
        let record = Record {
            member,
            channel,
            offset,
            event,
        };
        let mut out = vec![0; record.encoded_len().unwrap()];
        record.encode(&mut out).unwrap();
        out
    }

    /// A run asks for its agent-control endpoint with `shards.control=true`; `false`, or no
    /// such annotation, keeps step 5's endpoints; Docker's last value of a key wins, and any
    /// other value is refused by name.
    #[test]
    fn agent_control_is_asked_for_by_its_annotation_alone() {
        let asked = |values: &[&str]| {
            let annotations: Vec<String> = values
                .iter()
                .map(|v| format!("{CONTROL_ANNOTATION}={v}"))
                .collect();
            control_asked(&annotations)
        };
        assert_eq!(control_asked(&[]), Ok(false));
        assert_eq!(control_asked(&["other=true".into()]), Ok(false));
        assert_eq!(asked(&["true"]), Ok(true));
        assert_eq!(asked(&["false"]), Ok(false));
        assert_eq!(asked(&["true", "false"]), Ok(false));
        assert_eq!(asked(&["maybe", "true"]), Ok(true));
        for value in ["", "1", "yes", "TRUE"] {
            let refused = asked(&[value]).unwrap_err();
            assert!(refused.contains(CONTROL_ANNOTATION), "{refused}");
        }
    }

    /// The run's agent-control endpoint has a key of its own, made for this run: two runs' are
    /// two keys. It serves the run's owner alone, its lanes the run's streams: the owner's
    /// controller reads a member's record through it, and a stranger reaches nothing.
    #[test]
    fn a_runs_agent_control_endpoint_has_its_own_key_and_serves_its_owner_alone() {
        use shards_remote::control::{Controller, Ended, Fault, Got, Subscriptions};
        let owner = identity::Identity::generate().unwrap();
        let scope = Scope {
            run: RunId([3; 32]),
            generation: 1,
            member: 0,
            channel: Channel::Stdout,
        };
        let out = admitted(64);
        let (endpoint, served) =
            bind_agent(owner.fingerprint(), &[(scope, out)], &[], exec_admitted()).unwrap();
        let (other, _other_served) =
            bind_agent(owner.fingerprint(), &[(scope, out)], &[], exec_admitted()).unwrap();
        assert_ne!(endpoint.fingerprint, other.fingerprint, "two runs share a key");
        assert_ne!(endpoint.fingerprint, owner.fingerprint());
        let (vm, address) = (endpoint.fingerprint, endpoint.address);
        let serving = std::thread::spawn(move || endpoint.run());
        let subscriptions = [(scope, out, 0)];
        let dial = |own: &identity::Identity| {
            let (controller, dialed) = Controller::dial(
                own,
                vm,
                address,
                Subscriptions {
                    outputs: &subscriptions,
                    inputs: &[],
                    execs: None,
                    sockets: None,
                },
            )
            .unwrap();
            let driver = std::thread::spawn(move || controller.run());
            (dialed, driver)
        };
        let stranger = identity::Identity::generate().unwrap();
        let (mut refused, driver) = dial(&stranger);
        assert!(matches!(
            refused.outputs[0].next_record(),
            Err(Fault::Lost(Ended::NotEnrolled(_)))
        ));
        driver.join().unwrap().unwrap();
        let (mut dialed, _driver) = dial(&owner);
        let (producer, _control) = &served.outputs[0];
        producer.wait_capacity().unwrap();
        let mut record = producer.take_buffer().unwrap().unwrap();
        record.extend_from_slice(b"{\"type\":\"result\"}\xff");
        producer.submit(record).unwrap();
        assert_eq!(
            dialed.outputs[0].next_record().unwrap(),
            Got::Record {
                at: 0,
                bytes: b"{\"type\":\"result\"}\xff".to_vec()
            }
        );
        served.revoke.revoke();
        serving.join().unwrap().unwrap();
    }

    /// The admissions a run's execs get, as `start_managed_capture` gives them.
    fn exec_admitted() -> control::exec::Admissions {
        let input = Admitted {
            transport_bytes: u64::try_from(input::TAKEN_BYTES).unwrap(),
            ..admitted(INPUT_ROOM)
        };
        exec_admissions(admitted(64), input).unwrap()
    }

    /// What a run's exec host hands its VM process (`Daemon::launch_exec`): the member's exec,
    /// its flags, its stdin, stdout and stderr, and its client's end of its connection.
    type Handed = (run::Spec, u8, [shards_ipc::fd::OwnedFd; 3], Stream);

    /// The run's exec host (D238): through the run's agent-control endpoint its owner's
    /// controller execs into member 0, and the host hands its VM process the member's exec,
    /// told and reading its stdin (`EXEC_TOLD`, `EXEC_INTERACTIVE`, its setup `in-member=0`
    /// alone), the run's budget charged while it runs. The controller's stdin reaches the
    /// exec's pipe exactly and its output comes back exactly; a resize and a signal reach the VM
    /// process as `RESIZE` and `SIGNAL`; its stdin's end is told, and its exit once its output
    /// has finished, the charge given back after. An exec its member refuses, one whose spec
    /// sets a user, and one the budget has no room for are each told why, typed; the last two
    /// hand its VM process nothing.
    #[test]
    fn a_runs_exec_host_runs_a_members_exec_through_its_vm_process() {
        use shards_remote::control::exec::Unstarted;
        use shards_remote::control::{Controller, Exec, Fault, Got, Subscriptions};
        use std::io::{Read as _, Write as _};
        // A bound on what comes at once or never.
        const AT_ONCE: std::time::Duration = std::time::Duration::from_secs(5);
        let owner = identity::Identity::generate().unwrap();
        let scope = Scope {
            run: RunId([3; 32]),
            generation: 1,
            member: 0,
            channel: Channel::Stdout,
        };
        let out = admitted(64);
        let admissions = exec_admitted();
        let (endpoint, served) = bind_agent(owner.fingerprint(), &[(scope, out)], &[], admissions).unwrap();
        let (vm, address) = (endpoint.fingerprint, endpoint.address);
        let starts = served.execs.unwrap();
        let serving = std::thread::spawn(move || endpoint.run());
        let budget = Arc::new(Mutex::new(Some(0)));
        let charge = Charge::reserve(&budget, 0).unwrap();
        let (hand, handed) = std::sync::mpsc::channel::<Handed>();
        let launched = std::sync::atomic::AtomicUsize::new(0);
        let launch = |spec: &run::Spec,
                      _: &[String],
                      flags: u8,
                      stdio: [shards_ipc::fd::BorrowedFd<'_>; 3],
                      theirs: &Stream|
         -> io::Result<()> {
            launched.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let [stdin, stdout, stderr] = stdio;
            let fds = [
                stdin.try_clone_to_owned()?,
                stdout.try_clone_to_owned()?,
                stderr.try_clone_to_owned()?,
            ];
            hand.send((spec.clone(), flags, fds, theirs.try_clone()?))
                .map_err(io::Error::other)
        };
        let line = b"\x00\xff typed, no newline";
        let nowhere = shards_testdir::TempDir::new("exec-logs").unwrap();
        let logs = log_lanes::Logs {
            dir: &nowhere,
            active: &[0],
            drain: None,
            run_end: &|| Ok(None),
            forget: &|_| {},
        };
        std::thread::scope(|s| {
            let (charged, launcher, logs) = (&charge, &launch, &logs);
            let hosting = s.spawn(move || execs::host(&starts, admissions, charged, launcher, logs));
            // Its VM process: the first exec it runs, an echo of its stdin; the second its member
            // refuses.
            let process = s.spawn(move || {
                let (spec, flags, [stdin, stdout, stderr], conn) = handed.recv_timeout(AT_ONCE).unwrap();
                assert_eq!(flags, shards_ipc::EXEC_TOLD | shards_ipc::EXEC_INTERACTIVE);
                assert_eq!(spec.setup, vec![b"in-member=0".to_vec()]);
                assert_eq!(spec.argv, vec![b"cat".to_vec()]);
                shards_ipc::send(&conn, shards_ipc::kind::STARTED, &[], &[]).unwrap();
                let (mut stdin, mut stdout) = (File::from(stdin), File::from(stdout));
                let mut came = vec![0; line.len()];
                stdin.read_exact(&mut came).unwrap();
                stdout.write_all(&came).unwrap();
                let heard = |kind: u8| {
                    let said = shards_ipc::recv(&conn).unwrap().unwrap();
                    assert_eq!(said.kind, kind);
                    said.payload
                };
                assert_eq!(
                    heard(shards_ipc::kind::RESIZE),
                    run::Size { rows: 40, cols: 100 }.encode()
                );
                assert_eq!(heard(shards_ipc::kind::SIGNAL), 10u32.to_be_bytes());
                assert_eq!(stdin.read(&mut [0; 1]).unwrap(), 0, "its stdin's end");
                drop((stdin, stdout, stderr));
                shards_ipc::send(&conn, shards_ipc::kind::EXIT, &[7], &[]).unwrap();
                let (_, _, _, conn) = handed.recv_timeout(AT_ONCE).unwrap();
                let refusal = run::members::Refusal::Ended.encode(b"member 0 has ended");
                let said = [&[run::exec_failed::MEMBER][..], &refusal[..]].concat();
                shards_ipc::send(&conn, shards_ipc::kind::EXEC_UNSTARTED, &said, &[]).unwrap();
                shards_ipc::send(&conn, shards_ipc::kind::EXIT, &[1], &[]).unwrap();
            });
            let (controller, dialed) = Controller::dial(
                &owner,
                vm,
                address,
                Subscriptions {
                    outputs: &[(scope, out, 0)],
                    inputs: &[],
                    execs: Some(admissions),
                    sockets: None,
                },
            )
            .unwrap();
            let driver = s.spawn(move || controller.run());
            let execs = dialed.execs.as_ref().unwrap();
            let spec = run::Spec {
                argv: vec![b"cat".to_vec()],
                stdin: true,
                ..run::Spec::default()
            };
            let Exec {
                mut stdout,
                stderr,
                stdin,
                mut control,
            } = execs.exec(0, spec.clone()).unwrap();
            let (mut stderr, mut stdin) = (stderr.unwrap(), stdin.unwrap());
            assert_eq!(stdin.write(line).unwrap(), line.len());
            assert_eq!(
                stdout.next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: line.to_vec()
                }
            );
            assert!(*lock(&charge.bytes) > 0, "an exec running is charged");
            control.resize(run::Size { rows: 40, cols: 100 }).unwrap();
            control.signal(10).unwrap();
            assert_eq!(
                stdin.end().unwrap(),
                (line.len() as u64, run::managed::InputEnd::Closed)
            );
            assert_eq!(stdout.next_record().unwrap(), Got::Finished);
            assert_eq!(stderr.next_record().unwrap(), Got::Finished);
            assert_eq!(control.wait(), Ok(7));
            assert_eq!(
                execs.exec(0, spec.clone()).map(|_| ()),
                Err(Fault::Unstarted(Unstarted::Refused))
            );
            process.join().unwrap();
            let as_root = run::Spec {
                user: b"0".to_vec(),
                ..spec.clone()
            };
            assert_eq!(
                execs.exec(0, as_root).map(|_| ()),
                Err(Fault::Unstarted(Unstarted::Refused))
            );
            *lock(&budget) = Some(u64::MAX);
            assert_eq!(
                execs.exec(0, spec).map(|_| ()),
                Err(Fault::Unstarted(Unstarted::Capacity))
            );
            // An MCP connection (D254): no run of the daemon's asks for a bridge yet, so its
            // member has none, refused before anything reaches its VM process.
            assert_eq!(
                execs.mcp(0).map(|_| ()),
                Err(Fault::Unstarted(Unstarted::Refused))
            );
            served.revoke.revoke();
            serving.join().unwrap().unwrap();
            hosting.join().unwrap();
            let _ = driver.join();
        });
        assert_eq!(
            launched.into_inner(),
            2,
            "an exec refused here reached its VM process"
        );
        assert_eq!(*lock(&charge.bytes), 0, "an exec's charge outlived it");
    }

    /// A writer of the log in `dir`, as a run's VM writes its command's (segments of `size`
    /// bytes, as many as `files`).
    fn log_writer(dir: &Path, size: u64, files: u64) -> crate::log_writer::Writer {
        let newest = crate::daemon::newest_log(dir).unwrap();
        let held = shards_vmm::platform::open_dir(dir).unwrap();
        crate::log_writer::Writer::new(
            newest.log,
            newest.index,
            newest.seq,
            LogRetention { size, files },
            Box::new(move |seq| crate::segments::new_segment(&held, seq, files)),
        )
        .unwrap()
    }

    /// The run's log host (D238's log lane): through the run's agent-control endpoint its
    /// owner's controller reads the run's command's log and a drained member's, each record
    /// exact at its place, from the oldest kept; a place older than that is refused `Rotated`,
    /// naming the oldest kept, and a member that runs no program `Unknown`; a member's log
    /// followed from where an earlier lane got to gives what is kept as it is kept, and ends
    /// with its member's channels; the run's budget is charged while a lane reads and given
    /// back after.
    #[test]
    fn a_runs_log_host_serves_its_logs_from_the_store() {
        use crate::spec::{LOG_STDERR, LOG_STDOUT};
        use shards_remote::control::exec::{LogChannels, LogCursor, LogPos, LogRecord, LogSource, Unlogged};
        use shards_remote::control::{Controller, Fault, Logged, Subscriptions};
        let owner = identity::Identity::generate().unwrap();
        let run = shards_testdir::TempDir::new("log-host").unwrap();
        // The command's log, in segments of 30 bytes, two kept: its first let go.
        let mut command = log_writer(&run, 30, 2);
        command.keep(LOG_STDOUT, b"cmd out\n");
        command.keep(LOG_STDERR, b"cmd err\n");
        command.keep(LOG_STDOUT, b"third\n");
        let labels = vec![
            "agent files".to_string(),
            "agent main".to_string(),
            "agent idle".to_string(),
        ];
        let keep = LogRetention {
            size: 1 << 20,
            files: 2,
        };
        let drain = drain::Drain::open(&run, &labels, &[1], Some(keep), 64).unwrap();
        drain
            .keep(&encoded(1, Channel::Stdout, 0, Event::Data(b"kept\n")))
            .unwrap();
        let scope = Scope {
            run: RunId([3; 32]),
            generation: 1,
            member: 0,
            channel: Channel::Stdout,
        };
        let out = admitted(64);
        let admissions = exec_admitted();
        let (endpoint, served) = bind_agent(owner.fingerprint(), &[(scope, out)], &[], admissions).unwrap();
        let (vm, address) = (endpoint.fingerprint, endpoint.address);
        let starts = served.execs.unwrap();
        let serving = std::thread::spawn(move || endpoint.run());
        let budget = Arc::new(Mutex::new(Some(0)));
        let charge = Charge::reserve(&budget, 0).unwrap();
        let launch = |_: &run::Spec,
                      _: &[String],
                      _: u8,
                      _: [shards_ipc::fd::BorrowedFd<'_>; 3],
                      _: &Stream|
         -> io::Result<()> { Err(io::Error::other("no exec here")) };
        let logs = log_lanes::Logs {
            dir: &run,
            active: &[0, 1],
            drain: Some(&drain),
            run_end: &|| Ok(None),
            forget: &|_| {},
        };
        // What a lane gave, its times aside: an output's channel, place and bytes.
        let output = |logged: Logged| match logged {
            Logged::Record(LogRecord::Output {
                channel, pos, bytes, ..
            }) => Some((channel, pos, bytes)),
            _ => None,
        };
        let read_all = |log: &mut shards_remote::control::Log| {
            let mut got = Vec::new();
            loop {
                match log.next_record().unwrap() {
                    Logged::Finished => return got,
                    logged => got.push(output(logged).unwrap()),
                }
            }
        };
        let at = |seq, no, byte| LogPos { seq, no, byte };
        std::thread::scope(|s| {
            let (charged, launcher, logs) = (&charge, &launch, &logs);
            let hosting = s.spawn(move || execs::host(&starts, admissions, charged, launcher, logs));
            let (controller, dialed) = Controller::dial(
                &owner,
                vm,
                address,
                Subscriptions {
                    outputs: &[(scope, out, 0)],
                    inputs: &[],
                    execs: Some(admissions),
                    sockets: None,
                },
            )
            .unwrap();
            let driver = s.spawn(move || controller.run());
            let execs = dialed.execs.as_ref().unwrap();
            let mut ran = execs
                .log(LogSource::Command, LogChannels::BOTH, LogCursor::OLDEST, false)
                .unwrap();
            assert_eq!(
                read_all(&mut ran),
                [
                    (Channel::Stderr, at(1, 0, 0), b"cmd err\n".to_vec()),
                    (Channel::Stdout, at(2, 0, 0), b"third\n".to_vec()),
                ]
            );
            let older = LogCursor {
                stdout: Some(at(0, 0, 0)),
                stderr: None,
            };
            assert_eq!(
                execs
                    .log(LogSource::Command, LogChannels::BOTH, older, false)
                    .map(|_| ()),
                Err(Fault::Unlogged(Unlogged::Rotated { oldest: at(1, 0, 0) }))
            );
            assert_eq!(
                execs
                    .log(LogSource::Member(2), LogChannels::BOTH, LogCursor::OLDEST, false)
                    .map(|_| ()),
                Err(Fault::Unlogged(Unlogged::Unknown))
            );
            let mut member = execs
                .log(LogSource::Member(1), LogChannels::BOTH, LogCursor::OLDEST, false)
                .unwrap();
            assert_eq!(
                read_all(&mut member),
                [(Channel::Stdout, at(0, 0, 0), b"kept\n".to_vec())]
            );
            // Followed from where it got to: what is kept as it is kept, then its end.
            let mut followed = execs
                .log(LogSource::Member(1), LogChannels::BOTH, member.cursor(), true)
                .unwrap();
            assert!(*lock(&charge.bytes) > 0, "a log lane reading is charged");
            drain
                .keep(&encoded(1, Channel::Stderr, 0, Event::Data(b"\x00\xff more")))
                .unwrap();
            assert_eq!(
                output(followed.next_record().unwrap()),
                Some((Channel::Stderr, at(0, 1, 0), b"\x00\xff more".to_vec()))
            );
            for channel in [Channel::Stdout, Channel::Stderr] {
                drain.keep(&encoded(1, channel, 0, Event::End)).unwrap();
            }
            assert_eq!(followed.next_record().unwrap(), Logged::Finished);
            served.revoke.revoke();
            serving.join().unwrap().unwrap();
            hosting.join().unwrap();
            let _ = driver.join();
        });
        assert_eq!(*lock(&charge.bytes), 0, "a log lane's charge outlived it");
    }

    /// What a run's member host hands its VM process (`Daemon::launch_builtin`): the built-in's
    /// spec, its client's end of its connection, and its stdin, stdout and stderr.
    type Built = (run::Spec, Stream, [shards_ipc::fd::OwnedFd; 3]);

    /// The run's member host (D238): through the run's agent-control endpoint its owner's
    /// controller controls member 0, told it runs and its lane charged while it is open; a
    /// signal goes to its VM process as init's SIGNAL built-in with the member and the signal,
    /// no client's exec, and is taken where it exits 0 and refused `Ended` where it exits
    /// SIGNAL_ENDED; a resize of a member with no terminal of its own is refused `Terminal`,
    /// handing nothing; its end comes as its init tells it, after which every steer is refused
    /// `Ended`. Member 1, which ended before its lane asked, is told so; a lane the budget has no
    /// room for is refused `Capacity`; and every charge is given back.
    #[test]
    fn a_runs_member_host_tells_a_members_state_steers_it_and_tells_its_end() {
        use shards_remote::control::exec::{Unstarted, Unsteered};
        use shards_remote::control::{Controller, Fault, Member, MemberControl, Subscriptions};
        // A bound on what comes at once or never.
        const AT_ONCE: std::time::Duration = std::time::Duration::from_secs(5);
        let owner = identity::Identity::generate().unwrap();
        let scope = |member| Scope {
            run: RunId([3; 32]),
            generation: 1,
            member,
            channel: Channel::Stdout,
        };
        let out = admitted(64);
        let admissions = exec_admitted();
        let (endpoint, served) = bind_agent(
            owner.fingerprint(),
            &[(scope(0), out), (scope(1), out)],
            &[],
            admissions,
        )
        .unwrap();
        let (vm, address) = (endpoint.fingerprint, endpoint.address);
        let control::Served {
            revoke,
            members: lanes,
            ..
        } = served;
        let lanes = lanes.unwrap();
        let serving = std::thread::spawn(move || endpoint.run());
        let budget = Arc::new(Mutex::new(Some(0)));
        let charge = Charge::reserve(&budget, 0).unwrap();
        let ends = members::Ends::new(2).unwrap();
        ends.told(1, 0).unwrap();
        let (hand, handed) = std::sync::mpsc::channel::<Built>();
        let built = std::sync::atomic::AtomicUsize::new(0);
        let builtin = |spec: &run::Spec,
                       stdio: [shards_ipc::fd::BorrowedFd<'_>; 3],
                       theirs: &Stream|
         -> io::Result<()> {
            built.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let [stdin, stdout, stderr] = stdio;
            let fds = [
                stdin.try_clone_to_owned()?,
                stdout.try_clone_to_owned()?,
                stderr.try_clone_to_owned()?,
            ];
            hand.send((spec.clone(), theirs.try_clone()?, fds))
                .map_err(io::Error::other)
        };
        std::thread::scope(|s| {
            let hosting = members::Hosting {
                ends: &ends,
                terminals: &[],
                charge: &charge,
                started: &|| true,
                builtin: &builtin,
            };
            let host = s.spawn(move || members::host(&lanes, &hosting));
            // Its VM process: the first SIGNAL it is handed exits 0, the second SIGNAL_ENDED.
            let process = s.spawn(move || {
                for status in [0, run::builtin::SIGNAL_ENDED] {
                    let (spec, conn, stdio) = handed.recv_timeout(AT_ONCE).unwrap();
                    assert_eq!(spec.builtin, run::builtin::SIGNAL);
                    assert_eq!(spec.argv, run::members::signal_args(0, 15));
                    assert!(spec.setup.is_empty() && spec.user.is_empty());
                    drop(stdio);
                    shards_ipc::send(&conn, shards_ipc::kind::STARTED, &[], &[]).unwrap();
                    shards_ipc::send(&conn, shards_ipc::kind::EXIT, &[status], &[]).unwrap();
                }
            });
            let (controller, dialed) = Controller::dial(
                &owner,
                vm,
                address,
                Subscriptions {
                    outputs: &[(scope(0), out, 0), (scope(1), out, 0)],
                    inputs: &[],
                    execs: Some(admissions),
                    sockets: None,
                },
            )
            .unwrap();
            let driver = s.spawn(move || controller.run());
            let execs = dialed.execs.as_ref().unwrap();
            let Member::Running(MemberControl {
                mut steering,
                mut end,
            }) = execs.member(0).unwrap()
            else {
                panic!("member 0 runs");
            };
            assert!(*lock(&charge.bytes) > 0, "a member's lane open is charged");
            assert_eq!(steering.signal(15), Ok(()));
            assert_eq!(steering.signal(15), Err(Fault::Unsteered(Unsteered::Ended)));
            assert_eq!(
                steering.resize(run::Size { rows: 24, cols: 80 }),
                Err(Fault::Unsteered(Unsteered::Terminal))
            );
            process.join().unwrap();
            ends.told(0, 143).unwrap();
            assert_eq!(end.wait(), Ok(143));
            assert_eq!(steering.signal(9), Err(Fault::Unsteered(Unsteered::Ended)));
            assert!(matches!(execs.member(1), Ok(Member::Ended(0))));
            *lock(&budget) = Some(u64::MAX);
            assert_eq!(
                execs.member(0).map(|_| ()),
                Err(Fault::Unstarted(Unstarted::Capacity))
            );
            revoke.revoke();
            serving.join().unwrap().unwrap();
            host.join().unwrap();
            let _ = driver.join();
        });
        assert_eq!(
            built.into_inner(),
            2,
            "a steer refused here reached its VM process"
        );
        assert_eq!(*lock(&charge.bytes), 0, "a member's lane's charge outlived it");
    }

    /// A run's annotation gives members a terminal of their own by kind and name, as it gives
    /// them an input: each one that runs a program, once.
    #[test]
    fn terminal_members_are_named_as_inputs_are() {
        let labels: Vec<String> = vec!["agent files".into(), "harness driver".into()];
        let active = [1];
        let named =
            |value: &str| terminal_members(&[format!("{TERMINAL_ANNOTATION}={value}")], &labels, &active);
        assert_eq!(terminal_members(&[], &labels, &active), Ok(Vec::new()));
        assert_eq!(named("harness:driver"), Ok(vec![1]));
        for refused in ["agent:files", "harness:nobody", "harness:driver,harness:driver"] {
            assert!(named(refused).is_err(), "{refused}");
        }
    }

    /// The run's annotation names its input members by kind and name, each one that runs a
    /// program, once; a member it does not name has none. Docker's last value of a key wins.
    #[test]
    fn input_members_are_named_by_kind_and_name_and_each_runs_a_program() {
        let labels: Vec<String> = vec![
            "agent files".into(),
            "agent helper".into(),
            "harness driver".into(),
        ];
        let active = [1, 2];
        let named = |value: &str| input_members(&[format!("{STDIN_ANNOTATION}={value}")], &labels, &active);
        assert_eq!(input_members(&[], &labels, &active), Ok(Vec::new()));
        assert_eq!(
            input_members(&["other=harness:driver".into()], &labels, &active),
            Ok(Vec::new())
        );
        assert_eq!(named("harness:driver"), Ok(vec![2]));
        assert_eq!(named("harness:driver, agent:helper"), Ok(vec![1, 2]));
        for refused in [
            "agent:files",
            "harness:nobody",
            "harness:driver,harness:driver",
            "driver",
            "harness:",
            "tool:driver",
            "",
        ] {
            assert!(named(refused).is_err(), "{refused:?}");
        }
        let twice = [
            format!("{STDIN_ANNOTATION}=agent:files"),
            format!("{STDIN_ANNOTATION}=harness:driver"),
        ];
        assert_eq!(input_members(&twice, &labels, &active), Ok(vec![2]));
    }

    /// A member's input connection comes once; a second, or one after the input has gone, is
    /// ended; its relay, waiting for the connection, lets go as the input is let go, which
    /// ends the connection too, its peer reading its end.
    #[cfg(unix)]
    #[test]
    fn an_inputs_connection_comes_once_and_its_revocation_lets_its_relay_go() {
        let server = identity::Identity::generate().unwrap();
        let scope = input::Scope {
            run: RunId([7; 32]),
            generation: 1,
            member: 0,
        };
        let admitted = Admitted {
            record_bytes: INPUT_ROOM,
            transport_bytes: input::TAKEN_BYTES as u64,
            handshake_bytes: HANDSHAKE_BYTES,
            datagram_bytes: shards_remote::managed::DATAGRAM_BYTES,
        };
        let (endpoint, relay, control) =
            input::Endpoint::bind(&server, server.fingerprint(), scope, admitted).unwrap();
        let dir = shards_testdir::TempDir::new("managed-input").unwrap();
        let file = File::create(dir.join("metadata")).unwrap();
        let input = Arc::new(Input {
            scope,
            address: endpoint.address,
            control,
            guest: Mutex::new(Guest::Awaited),
            arrived: Condvar::new(),
            metadata: shared_metadata(file).unwrap(),
            recorded: std::sync::atomic::AtomicBool::new(false),
        });
        let closing = Arc::downgrade(&input);
        input.control.on_close(move || {
            if let Some(input) = closing.upgrade() {
                input.close();
            }
        });
        let (ours, mut theirs) = Stream::pair().unwrap();
        input.arrive(ours).unwrap();
        let (again, mut second) = Stream::pair().unwrap();
        assert!(input.arrive(again).is_err(), "a second connection is ended");
        let mut byte = [0];
        assert_eq!(second.read(&mut byte).unwrap(), 0);
        assert!(input.wait().is_some());
        std::thread::scope(|threads| {
            let relaying = threads.spawn(|| relay_input(&input, &relay));
            input.control.abort();
            relaying.join().unwrap();
        });
        assert_eq!(theirs.read(&mut byte).unwrap(), 0, "its connection ended with it");
        assert!(matches!(*lock(&input.guest), Guest::Gone));
        let (late, mut third) = Stream::pair().unwrap();
        assert!(input.arrive(late).is_err());
        assert_eq!(third.read(&mut byte).unwrap(), 0);
        // Its end, revoked before its init told any, is recorded once.
        input.record();
        input.record();
        assert_eq!(
            std::fs::read_to_string(dir.join("metadata")).unwrap(),
            "member 0 channel 0 state revoked written 0 errno 0\n"
        );
        drop(endpoint);
    }

    #[test]
    fn image_identity_and_channel_offsets_are_checked_independently() {
        let labels = vec!["harness first".into(), "agent second".into()];
        let mut states = [StreamState::New; 4];
        assert!(
            accept(
                &encoded(2, Channel::Stdout, 0, Event::Open("agent second")),
                &labels,
                &mut states
            )
            .is_err()
        );
        assert!(
            accept(
                &encoded(1, Channel::Stdout, 0, Event::Open("harness first")),
                &labels,
                &mut states
            )
            .is_err()
        );
        for channel in [Channel::Stdout, Channel::Stderr] {
            assert!(
                accept(
                    &encoded(1, channel, 0, Event::Open("agent second")),
                    &labels,
                    &mut states
                )
                .is_ok()
            );
        }
        assert!(
            accept(
                &encoded(1, Channel::Stdout, 0, Event::Data(b"\0\xff\r\n")),
                &labels,
                &mut states
            )
            .is_ok()
        );
        assert!(
            accept(
                &encoded(1, Channel::Stderr, 4, Event::Data(b"bad offset")),
                &labels,
                &mut states
            )
            .is_err()
        );
        assert!(
            accept(
                &encoded(1, Channel::Stderr, 0, Event::Data(b"stderr")),
                &labels,
                &mut states
            )
            .is_ok()
        );
        assert!(accept(&encoded(1, Channel::Stdout, 4, Event::End), &labels, &mut states).is_ok());
        assert!(
            accept(
                &encoded(1, Channel::Stdout, 4, Event::Data(b"past end")),
                &labels,
                &mut states
            )
            .is_err()
        );
        assert!(!states.get(3).unwrap().terminal());
    }

    #[test]
    fn a_blocked_channel_does_not_take_another_channels_credit() {
        let at = Progress {
            accepted: 0,
            consumed: 0,
        };
        let queue = Credits {
            state: Mutex::new(Queue {
                slots: vec![
                    CreditSlot::Captured(vec![1], After::More),
                    CreditSlot::Waiting,
                    CreditSlot::Waiting,
                    CreditSlot::Waiting,
                ],
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        };
        let observe = |index| {
            Ok(if index == 1 || index == 3 {
                State::Ready(at)
            } else {
                State::Pending(at)
            })
        };
        assert!(matches!(queue.next(observe).unwrap(), Some(Action::Grant(1))));
        assert!(matches!(queue.next(observe).unwrap(), Some(Action::Grant(3))));
        queue.wake();
        assert!(matches!(
            queue.next(|_| Ok(State::Ready(at))).unwrap(),
            Some(Action::Submit(0, _, After::More))
        ));
        queue.close();
        assert!(queue.next(observe).unwrap().is_none());
    }

    #[test]
    fn generation_is_persisted_and_never_wraps_or_reuses_a_bad_record() {
        let dir = shards_testdir::TempDir::new("managed-gen").unwrap();
        assert_eq!(generation(&dir).unwrap(), 1);
        assert_eq!(generation(&dir).unwrap(), 2);
        std::fs::write(dir.join(GENERATION), u64::MAX.to_be_bytes()).unwrap();
        assert!(generation(&dir).is_err());
        std::fs::write(dir.join(GENERATION), [0, 1]).unwrap();
        assert!(generation(&dir).is_err());
    }

    #[test]
    fn an_unknown_remote_owner_never_falls_back_to_the_home_key() {
        let dir = shards_testdir::TempDir::new("managed-key").unwrap();
        assert_eq!(owner_key(&dir, "", [7; 32]).unwrap(), [7; 32]);
        assert!(owner_key(&dir, "missing", [7; 32]).is_err());
    }

    #[test]
    fn the_initial_record_fits_actual_metadata_and_terminal_errno() {
        let labels = vec!["agent a".into(), "harness a longer supplied name".into()];
        let capacity = record_capacity(&labels, &[0, 1]).unwrap();
        for label in &labels {
            assert!(encoded(0, Channel::Stdout, 0, Event::Open(label)).len() <= capacity);
        }
        assert!(encoded(0, Channel::Stderr, 0, Event::Aborted(5)).len() <= capacity);
    }

    /// A run's record (its first line) and two of its channels' terminal records: member 0's
    /// stdout aborted with a canonical errno, then its stderr ended.
    #[cfg(unix)]
    const TORN_HEADER: &str = "shards managed output 1\n";
    #[cfg(unix)]
    const ABORTED_RECORD: &str = "member 0 channel 1 canonical aborted errno 5\n\
        member 0 channel 1 state aborted accepted 20 consumed 20 tail unknown\n";
    #[cfg(unix)]
    const ENDED_RECORD: &str = "member 0 channel 2 state ended accepted 7 consumed 7 tail complete\n";

    /// A channel's terminal record whose append fails part-way leaves the run's record of
    /// whole lines: the next channel's record starts a line of its own, and the failed one is
    /// there whole or not at all (finding 2 of audit-fix-host-v1: its part stayed, and the
    /// next channel's line was written onto it). Its child lowers its own process's file
    /// size limit partway through the first record, as a full disk cuts a write short, and
    /// raises it again before the second, as a disk's room comes back.
    #[cfg(unix)]
    #[test]
    fn a_terminal_record_that_fails_part_way_leaves_whole_lines() {
        let dir = shards_testdir::TempDir::new("managed-torn").unwrap();
        let path = dir.join("metadata");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon::managed::tests::torn_append_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SHARDS_MANAGED_TORN", &path)
            .output()
            .unwrap();
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(child.status.success(), "{said}");
        assert!(said.contains(" 1 passed"), "the child ran no case: {said}");
        let record = std::fs::read_to_string(&path).unwrap();
        let whole = [
            format!("{TORN_HEADER}{ENDED_RECORD}"),
            format!("{TORN_HEADER}{ABORTED_RECORD}{ENDED_RECORD}"),
        ];
        assert!(
            whole.contains(&record),
            "a terminal append that failed part-way left a torn line, the next channel's \
             written onto it:\n{record}"
        );
    }

    /// The child of `a_terminal_record_that_fails_part_way_leaves_whole_lines`, alone in its
    /// process, whose file size limit it sets.
    #[cfg(unix)]
    #[test]
    #[ignore = "child of a_terminal_record_that_fails_part_way_leaves_whole_lines: sets its process's RLIMIT_FSIZE"]
    fn torn_append_child() {
        let Some(path) = std::env::var_os("SHARDS_MANAGED_TORN") else {
            return;
        };
        let server = identity::Identity::generate().unwrap();
        let mut file = private_record(Path::new(&path)).unwrap();
        file.write_all(TORN_HEADER.as_bytes()).unwrap();
        let metadata = shared_metadata(file).unwrap();
        let charge = Arc::new(Charge {
            budget: Arc::new(Mutex::new(Some(0))),
            bytes: Mutex::new(0),
        });
        let capacity = record_capacity(&["agent".into()], &[0]).unwrap();
        let at = |n| Progress {
            accepted: n,
            consumed: n,
        };
        let [aborted, ended] = [
            (
                Channel::Stdout,
                Life::Aborted(Some(Terminal::Aborted(5))),
                State::Aborted(at(20)),
            ),
            (Channel::Stderr, Life::Ended, State::Finished(at(7))),
        ]
        .map(|(channel, life, state)| {
            let (scope, endpoint, _producer, control) =
                bound(&server, server.fingerprint(), channel, capacity);
            let output = output(
                scope,
                &endpoint,
                control,
                capture_quantum(capacity).unwrap(),
                capacity,
                &metadata,
                std::sync::Weak::new(),
                0,
                &charge,
            );
            // Resolved already, as `complete` resolves a channel before it publishes.
            *lock(&output.life) = life;
            (output, life, state)
        });
        // SAFETY: SIG_IGN for SIGXFSZ, so that a write past the limit fails with EFBIG
        // rather than ending this child.
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: reads this process's limit into a valid local.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) }, 0);
        let given = limit;
        limit.rlim_cur = libc::rlim_t::try_from(TORN_HEADER.len() + ABORTED_RECORD.len() / 2).unwrap();
        // SAFETY: lowers this process's own soft limit, from a valid local.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);
        aborted.0.publish(Some((aborted.1, aborted.2)));
        // SAFETY: restores this process's own soft limit, from a valid local.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &given) }, 0);
        ended.0.publish(Some((ended.1, ended.2)));
    }

    /// Test patience for the real transport below, as the daemon's own tests wait.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);

    fn admitted(capacity: usize) -> Admitted {
        Admitted {
            record_bytes: capacity,
            transport_bytes: u64::try_from(capacity + shards_remote::managed::DATA_HEAD).unwrap(),
            handshake_bytes: HANDSHAKE_BYTES,
            datagram_bytes: shards_remote::managed::DATAGRAM_BYTES,
        }
    }

    /// Member 0's `channel` of a run, bound as `start_managed_capture` binds it.
    fn bound(
        server: &identity::Identity,
        owner: Fingerprint,
        channel: Channel,
        capacity: usize,
    ) -> (Scope, Endpoint, Producer, Control) {
        let scope = Scope {
            run: RunId([7; 32]),
            generation: 1,
            member: 0,
            channel,
        };
        let (endpoint, producer, control) = Endpoint::bind(server, owner, scope, admitted(capacity)).unwrap();
        (scope, endpoint, producer, control)
    }

    /// The output of a bound channel, as `start_managed_capture` makes it, learning in
    /// `quantum`.
    #[allow(clippy::too_many_arguments)]
    fn output(
        scope: Scope,
        endpoint: &Endpoint,
        control: Control,
        quantum: shards_registry::Quantum,
        capacity: usize,
        metadata: &Arc<Mutex<Ledger>>,
        credits: std::sync::Weak<Credits>,
        index: usize,
        charge: &Arc<Charge>,
    ) -> Output {
        Output {
            scope,
            address: endpoint.address,
            control,
            metadata: Arc::clone(metadata),
            life: Mutex::new(Life::Serving),
            credits,
            index,
            learning: Mutex::new(Learning {
                quantum,
                sample: None,
                admitted: capacity,
            }),
            charge: Arc::clone(charge),
        }
    }

    /// Climbs `quantum` as an owner does that consumes each full record a millisecond after
    /// it was issued (D226's climb), until its window has not changed for 64 records (a
    /// doubling takes six: a window it settles in and two it measures, two records each);
    /// the window it desires then.
    fn climbed(quantum: &mut shards_registry::Quantum) -> usize {
        let start = Instant::now();
        let mut desired = quantum.desired();
        let mut unchanged = 0;
        for n in 0..100_000 {
            let began = start + std::time::Duration::from_millis(n);
            quantum.consumed(began, began + std::time::Duration::from_millis(1), desired as u64);
            if quantum.desired() == desired {
                unchanged += 1;
                if unchanged == 64 {
                    return desired;
                }
            } else {
                (desired, unchanged) = (quantum.desired(), 0);
            }
        }
        panic!("the quantum never settled: {desired}");
    }

    /// A growth its endpoint refuses (`Admission(Capacity)`) leaves the channel the window it
    /// has: it is credited at that, the charge the growth took is given back, and the channel
    /// serves on (finding 1 of audit-fix-host-v1: the refusal was an error that ended the
    /// owner's credit worker and, with it, every channel of the run).
    #[test]
    fn growth_the_endpoint_refuses_keeps_the_current_window() {
        let server = identity::Identity::generate().unwrap();
        let capacity = record_capacity(&["agent".into()], &[0]).unwrap();
        let (scope, endpoint, producer, control) =
            bound(&server, server.fingerprint(), Channel::Stdout, capacity);
        let storage = producer.storage_capacity();
        let dir = shards_testdir::TempDir::new("managed-refused").unwrap();
        let metadata = shared_metadata(File::create(dir.join("metadata")).unwrap()).unwrap();
        let budget = Arc::new(Mutex::new(Some(0)));
        let charge = Arc::new(Charge {
            budget: Arc::clone(&budget),
            bytes: Mutex::new(0),
        });
        // A quantum climbing past what one endpoint record holds: the capture's own, before
        // its bound.
        let quantum = shards_registry::Quantum::new(capacity, run::MAX_PAYLOAD as usize).unwrap();
        let output = output(
            scope,
            &endpoint,
            control,
            quantum,
            capacity,
            &metadata,
            std::sync::Weak::new(),
            0,
            &charge,
        );
        let desired = climbed(&mut lock(&output.learning).quantum);
        assert!(
            matches!(
                producer.admit(desired),
                Err(shards_remote::managed::Error::Admission(
                    shards_remote::managed::AdmissionError::Capacity
                ))
            ),
            "its endpoint holds a window of {desired}: no refusal to keep"
        );
        let granted = output.capacity(&producer);
        assert!(
            matches!(granted, Ok(n) if n == capacity),
            "a growth its endpoint refused cost the channel its credit: {granted:?}"
        );
        assert_eq!(lock(&output.learning).admitted, capacity);
        assert_eq!(producer.storage_capacity(), storage);
        assert_eq!(
            *lock(&budget),
            Some(0),
            "the refused growth's charge stayed taken"
        );
        assert_eq!(*lock(&output.life), Life::Serving);
        assert!(!matches!(output.control.state(), State::Aborted(_)));
    }

    /// An owner subscribed to one channel at its start over the real transport, as an
    /// owner's observer is: what a channel needs of its owner to be credited (`Ready`).
    struct Subscriber {
        scope: Scope,
        config: Option<hyper_quic::ClientConfig>,
        pending: Vec<u8>,
        at: Option<(shards_remote::net::ConnKey, hyper_quic::StreamId)>,
        ended: bool,
    }

    impl Subscriber {
        fn flush(&mut self, io: &mut shards_remote::net::Io<'_>) {
            let Some((conn, stream)) = self.at else { return };
            while !self.pending.is_empty() {
                match io.write(conn, stream, &self.pending) {
                    Ok(0) => return,
                    Ok(n) => drop(self.pending.drain(..n)),
                    Err(_) => {
                        self.ended = true;
                        return;
                    }
                }
            }
        }
    }

    impl shards_remote::net::Handler for Subscriber {
        type Command = std::net::SocketAddr;

        fn connected(
            &mut self,
            io: &mut shards_remote::net::Io<'_>,
            conn: shards_remote::net::ConnKey,
            _: &[u8],
        ) -> Result<(), shards_remote::net::Close> {
            let stream = io.open(conn).map_err(|_| shards_remote::net::Close {
                code: 1,
                why: "the subscription's stream",
            })?;
            self.at = Some((conn, stream));
            self.pending
                .extend_from_slice(&shards_remote::managed::subscribe(self.scope, 0));
            self.flush(io);
            Ok(())
        }
        fn opened(&mut self, _: &mut shards_remote::net::Io<'_>, _: u64, _: hyper_quic::StreamId) {}
        fn readable(&mut self, _: &mut shards_remote::net::Io<'_>, _: u64, _: hyper_quic::StreamId) {}
        fn writable(&mut self, io: &mut shards_remote::net::Io<'_>, _: u64, _: hyper_quic::StreamId) {
            self.flush(io);
        }
        fn stopped(&mut self, _: &mut shards_remote::net::Io<'_>, _: u64, _: hyper_quic::StreamId) {
            self.ended = true;
        }
        fn lost(&mut self, _: &mut shards_remote::net::Io<'_>, _: u64, _: &str, _: Option<u64>) {
            self.ended = true;
        }
        fn command(&mut self, io: &mut shards_remote::net::Io<'_>, to: std::net::SocketAddr) {
            match self.config.take().map(|config| io.connect(to, config)) {
                Some(Ok(_)) => {}
                _ => self.ended = true,
            }
        }
        fn done(&self) -> bool {
            self.ended
        }
    }

    /// The credit the owner's credit worker sends its capture, read from the capture's end.
    fn credit(peer: &Stream) -> io::Result<run::managed::Credit> {
        let mut head = [0; run::HEADER];
        read_exact(peer, &mut head)?;
        let (kind, len) = run::parse_header(head).ok_or_else(|| io::Error::other("not a frame"))?;
        if kind != run::kind::MANAGED_CREDIT {
            return Err(io::Error::other(format!("a frame of kind {kind}, not a credit")));
        }
        let mut payload = vec![0; len as usize];
        read_exact(peer, &mut payload)?;
        run::managed::Credit::decode(&payload).map_err(|e| io::Error::other(e.to_string()))
    }

    /// Do: a run whose one member no owner reads, both its channels drained; play its VM,
    /// answering each credit with a record of that channel (its open, then data as long as
    /// the credit allows, many credits' worth, then its end) with no subscriber anywhere.
    /// Expect each next credit to come all the same (D240: no member waits on a reader), every
    /// record's bytes kept whole in the member's log, each channel apart, and the capture's
    /// credit worker and bridge to end with the ends. A drain that waited on a reader would
    /// leave the VM's read of its next credit to time out.
    #[cfg(unix)]
    #[test]
    fn a_drained_channel_is_credited_again_as_soon_as_its_record_is_kept() {
        let run = shards_testdir::TempDir::new("drained-credit").unwrap();
        let labels: Vec<String> = vec!["agent main".into()];
        let capacity = record_capacity(&labels, &[0]).unwrap();
        let keep = LogRetention {
            size: 1 << 20,
            files: 2,
        };
        let drain = Arc::new(drain::Drain::open(&run, &labels, &[0], Some(keep), capacity).unwrap());
        let charge = Arc::new(Charge {
            budget: Arc::new(Mutex::new(Some(0))),
            bytes: Mutex::new(0),
        });
        let credits = Arc::new(Credits {
            state: Mutex::new(Queue {
                slots: vec![CreditSlot::Waiting, CreditSlot::Waiting],
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        });
        let (stream, peer) = Stream::pair().unwrap();
        peer.set_read_timeout(Some(PATIENCE)).unwrap();
        let vm = peer.try_clone().unwrap();
        let (writing, reading) = (stream.try_clone().unwrap(), stream.try_clone().unwrap());
        let owner = Owner {
            outputs: vec![None, None],
            inputs: vec![None],
            drain: Some(Arc::clone(&drain)),
            agent: None,
            stream,
            credits,
            charge,
            ends: Arc::new(members::Ends::new(1).unwrap()),
            told: Mutex::new(Told::Capturing),
        };
        // Many credits' worth of each channel, more than a pipe holds.
        let rounds = 64;
        let mut written: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
        /// Ends the capture's threads if the VM's play fails, as its VM's side drops with it:
        /// the failure is told, not waited on by the scope's threads.
        struct Unwound<'a>(&'a Credits);
        impl Drop for Unwound<'_> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    self.0.close();
                }
            }
        }
        let (credited, bridged) = std::thread::scope(|threads| {
            let (owner, labels) = (&owner, &labels);
            let crediting = threads.spawn(move || send_credits(writing, owner, &[]));
            let bridging = threads.spawn(move || bridge(reading, owner, labels, vec![StreamState::New; 2]));
            let (peer, mut vm, _unwound) = (peer, vm, Unwound(&owner.credits));
            let (mut offsets, mut sent, mut opened, mut ended) = ([0u64; 2], [0usize; 2], [false; 2], 0);
            while ended < 2 {
                let granted = credit(&peer).expect("a drained channel's next credit");
                let at = route_index(0, granted.channel).unwrap();
                let record = if !opened[at] {
                    opened[at] = true;
                    encoded(0, granted.channel, 0, Event::Open("agent main"))
                } else if sent[at] < rounds {
                    sent[at] += 1;
                    // A data record is its header and its bytes (an empty one is refused).
                    let room = usize::try_from(granted.capacity).unwrap() - run::managed::HEADER;
                    let bytes: Vec<u8> = (0..room).map(|i| (i + sent[at] + at) as u8).collect();
                    written[at].extend_from_slice(&bytes);
                    let record = encoded(0, granted.channel, offsets[at], Event::Data(&bytes));
                    offsets[at] += room as u64;
                    record
                } else {
                    ended += 1;
                    encoded(0, granted.channel, offsets[at], Event::End)
                };
                vm.write_all(&run::header(run::kind::MANAGED, record.len() as u32))
                    .unwrap();
                vm.write_all(&record).unwrap();
            }
            (crediting.join().unwrap(), bridging.join().unwrap())
        });
        credited.unwrap();
        bridged.unwrap();
        drain.close();
        let log = crate::daemon::logs::LogFile::open(&drain::member_dir(&run, 0)).unwrap();
        let mut kept: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
        let mut reader = crate::daemon::logs::Reader::new();
        let mut each = |p: crate::daemon::logs::Piece<'_>| {
            kept[usize::from(p.stream == crate::spec::LOG_STDERR)].extend_from_slice(p.bytes);
            Ok(true)
        };
        reader.read(&log, &mut each).unwrap();
        reader.finish(&mut each).unwrap();
        assert_eq!(kept[0].len(), written[0].len());
        assert!(
            kept == written,
            "the member's log keeps its channels' bytes whole and apart"
        );
    }

    /// A run of one member, `agent main`, whose output the daemon drains, as its capture's
    /// threads find it: the drain, kept in `run`, the owner, and the VM's end of its stream.
    #[cfg(unix)]
    fn drained_run(run: &Path) -> (Arc<drain::Drain>, Arc<Owner>, Stream, [Stream; 2]) {
        let labels: Vec<String> = vec!["agent main".into()];
        let capacity = record_capacity(&labels, &[0]).unwrap();
        let keep = LogRetention {
            size: 1 << 20,
            files: 2,
        };
        let drain = Arc::new(drain::Drain::open(run, &labels, &[0], Some(keep), capacity).unwrap());
        let credits = Arc::new(Credits {
            state: Mutex::new(Queue {
                slots: vec![CreditSlot::Waiting, CreditSlot::Waiting],
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        });
        let (stream, peer) = Stream::pair().unwrap();
        peer.set_read_timeout(Some(PATIENCE)).unwrap();
        let sides = [stream.try_clone().unwrap(), stream.try_clone().unwrap()];
        let owner = Arc::new(Owner {
            outputs: vec![None, None],
            inputs: vec![None],
            drain: Some(Arc::clone(&drain)),
            agent: None,
            stream,
            credits,
            charge: Arc::new(Charge {
                budget: Arc::new(Mutex::new(Some(0))),
                bytes: Mutex::new(0),
            }),
            ends: Arc::new(members::Ends::new(1).unwrap()),
            told: Mutex::new(Told::Capturing),
        });
        (drain, owner, peer, sides)
    }

    /// Plays a drained run's VM: answers each credit with the next record of its channel,
    /// `stdout`'s or `stderr`'s, each its frame, until both lists are spent; `before(channel,
    /// n)` before the `n`th record of a channel's (from 0) is sent. A credit for a channel
    /// whose list is spent goes unanswered.
    #[cfg(unix)]
    fn play(
        peer: &Stream,
        stdout: &[Event<'_>],
        stderr: &[Event<'_>],
        before: &mut dyn FnMut(Channel, usize),
    ) {
        let mut vm = peer.try_clone().unwrap();
        let lists = [stdout, stderr];
        let (mut sent, mut offsets) = ([0usize; 2], [0u64; 2]);
        while sent[0] < stdout.len() || sent[1] < stderr.len() {
            let granted = credit(peer).expect("a drained channel's next credit");
            let at = route_index(0, granted.channel).unwrap();
            let Some(&event) = lists[at].get(sent[at]) else {
                continue;
            };
            before(granted.channel, sent[at]);
            let record = encoded(0, granted.channel, offsets[at], event);
            if let Event::Data(bytes) = event {
                offsets[at] += bytes.len() as u64;
            }
            vm.write_all(&run::header(run::kind::MANAGED, record.len() as u32))
                .unwrap();
            vm.write_all(&record).unwrap();
            sent[at] += 1;
        }
    }

    /// What member 0's log in `run` keeps of each channel, its bytes whole.
    #[cfg(unix)]
    fn kept_in(run: &Path) -> [Vec<u8>; 2] {
        let log = crate::daemon::logs::LogFile::open(&drain::member_dir(run, 0)).unwrap();
        let mut kept: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
        let mut reader = crate::daemon::logs::Reader::new();
        let mut each = |p: crate::daemon::logs::Piece<'_>| {
            kept[usize::from(p.stream == crate::spec::LOG_STDERR)].extend_from_slice(p.bytes);
            Ok(true)
        };
        reader.read(&log, &mut each).unwrap();
        reader.finish(&mut each).unwrap();
        kept
    }

    /// Aborts a capture as its test fails within its threads' scope: its credit queue closed,
    /// the daemon's end of its stream ended (so its bridge's read ends, though the test's VM
    /// end is held past the scope) and its drain closed (so its viewer ends), so that the
    /// failure is told, never waited on by the scope's threads.
    #[cfg(unix)]
    struct AbortedOnPanic<'a>(&'a Owner);
    #[cfg(unix)]
    impl Drop for AbortedOnPanic<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.0.abort();
            }
        }
    }

    /// Do: a run whose one member the daemon drains, its VM played: the member's stdout
    /// opened and a line of it sent and kept; then the run's end (its DONE, `Outputs::end`);
    /// then the rest of its stdout and the ends of both its channels, as they come on the
    /// capture's own stream after its VM has told its run's end on its link. Expect each
    /// credit to come all the same, the member's log to keep every byte, and a viewer to show
    /// both lines: the run's end ends no capture whose records are on their way (D240).
    /// Before, it ended the capture's stream at once, and the rest of the member's output was
    /// lost: CI's agents' reports cut mid-line (`[agent a] conf`, PM M543).
    #[cfg(unix)]
    #[test]
    fn a_runs_end_lets_its_capture_keep_what_comes_after_it() {
        let run = shards_testdir::TempDir::new("drained-end").unwrap();
        let (drain, owner, peer, [writing, reading]) = drained_run(&run);
        let outputs = Outputs::default();
        lock(&outputs.runs).insert("run".into(), Entry::Serving(Arc::clone(&owner)));
        let labels: Vec<String> = vec!["agent main".into()];
        let (credited, bridged, shown) = std::thread::scope(|threads| {
            let (owned, labels) = (&*owner, &labels);
            let crediting = threads.spawn(move || send_credits(writing, owned, &[]));
            let bridging = threads.spawn(move || bridge(reading, owned, labels, vec![StreamState::New; 2]));
            let viewing = threads.spawn(|| {
                let mut to = Vec::new();
                drain.show(&mut to);
                to
            });
            let _aborted = AbortedOnPanic(owned);
            let (stdout, stderr) = (
                [
                    Event::Open("agent main"),
                    Event::Data(b"first\n"),
                    Event::Data(b"second\n"),
                    Event::End,
                ],
                [Event::Open("agent main"), Event::End],
            );
            // The run's end comes once its first line is kept (its stdout credited again),
            // before the rest is sent.
            let mut ended = false;
            play(&peer, &stdout, &stderr, &mut |channel, n| {
                if channel == Channel::Stdout && n == 2 {
                    let _ = outputs.end("run");
                    ended = true;
                }
            });
            assert!(ended, "the run's end came within the play");
            (
                crediting.join().unwrap(),
                bridging.join().unwrap(),
                viewing.join().unwrap(),
            )
        });
        credited.unwrap();
        bridged.unwrap();
        assert_eq!(kept_in(&run), [b"first\nsecond\n".to_vec(), Vec::new()]);
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            "[agent main] first\n[agent main] second\n"
        );
    }

    /// Do: a drained run's VM sends its member's stdout opened, a line and half of the
    /// next, and its stderr opened, then goes, its stream ended though neither channel has.
    /// Expect what it sent kept whole, the line cut short shown ended, and the capture's credit
    /// worker and bridge, as the daemon runs them, to end with nothing told: the capture is
    /// cut (`Owner::cut`), and its credit worker keeps what came before its drain ends.
    #[cfg(unix)]
    #[test]
    fn a_capture_cut_short_keeps_all_it_brought() {
        let run = shards_testdir::TempDir::new("drained-cut").unwrap();
        let (drain, owner, peer, [writing, reading]) = drained_run(&run);
        let labels: Vec<String> = vec!["agent main".into()];
        let (told, shown) = std::thread::scope(|threads| {
            let (owned, labels) = (&*owner, &labels);
            let crediting = threads.spawn(move || run_credits(writing, owned, &[]));
            let bridging =
                threads.spawn(move || run_bridge(reading, owned, labels, vec![StreamState::New; 2]));
            let viewing = threads.spawn(|| {
                let mut to = Vec::new();
                drain.show(&mut to);
                to
            });
            let _aborted = AbortedOnPanic(owned);
            play(
                &peer,
                &[
                    Event::Open("agent main"),
                    Event::Data(b"first\n"),
                    Event::Data(b"hal"),
                ],
                &[Event::Open("agent main")],
                &mut |_, _| {},
            );
            // Its VM gone, its end of the stream with it.
            drop(peer);
            let cut = bridging.join().unwrap();
            assert!(
                cut.is_some_and(|e| e.ended()),
                "its bridge says its VM's end of the stream, which stops no run (D258)"
            );
            (crediting.join().unwrap(), viewing.join().unwrap())
        });
        let (failed, told) = told;
        assert!(failed.is_none(), "its credit worker ends as its capture does");
        assert!(told.is_none(), "no run's end was held");
        assert_eq!(kept_in(&run), [b"first\nhal".to_vec(), Vec::new()]);
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            "[agent main] first\n[agent main] hal\n"
        );
    }

    /// A viewer's client that reads nothing: each write waits until it is let go.
    #[cfg(unix)]
    struct Stalled(std::sync::mpsc::Receiver<()>, Vec<u8>);
    #[cfg(unix)]
    impl io::Write for Stalled {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            // Its sender's drop lets every write go, at once.
            let _ = self.0.recv();
            self.1.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Do: a drained run whose attached client takes nothing of what it is shown (its
    /// viewer's every write waits), its run's end told while its capture still keeps, then the
    /// member's lines and ends. Expect every credit to come and every record to be kept, the
    /// run's end held (`RunEnd::tell_after`) until the capture's credit worker has made its
    /// last keep, and then given back by it, while the viewer still waits; and the viewer, let
    /// go at last, to show every line. Nothing waits on what is shown (M543), and the run's end
    /// waits on its members' logs alone.
    #[cfg(unix)]
    #[test]
    fn a_runs_end_waits_for_its_members_logs_and_never_for_its_viewer() {
        let run = shards_testdir::TempDir::new("drained-held").unwrap();
        let (drain, owner, peer, [writing, reading]) = drained_run(&run);
        let outputs = Outputs::default();
        lock(&outputs.runs).insert("run".into(), Entry::Serving(Arc::clone(&owner)));
        let labels: Vec<String> = vec!["agent main".into()];
        let ended = |status| super::super::Ended {
            pid: 1,
            started: true,
            status,
            said: None,
            saved: None,
            detached: None,
            named: false,
        };
        let (go, held) = std::sync::mpsc::channel::<()>();
        let (told, shown) = std::thread::scope(|threads| {
            let (owned, labels) = (&*owner, &labels);
            let (gave, given) = std::sync::mpsc::channel();
            threads.spawn(move || gave.send(run_credits(writing, owned, &[])));
            threads.spawn(move || run_bridge(reading, owned, labels, vec![StreamState::New; 2]));
            let viewing = threads.spawn(|| {
                let mut to = Stalled(held, Vec::new());
                drain.show(&mut to);
                to.1
            });
            // A failure lets the viewer go, so that the scope ends.
            let go = Some(go);
            let _aborted = AbortedOnPanic(owned);
            let capturing = outputs.end("run").expect("a managed scope");
            assert!(
                capturing.tell_after(ended(7)).is_none(),
                "the run's end waits for its capture's"
            );
            play(
                &peer,
                &[Event::Open("agent main"), Event::Data(b"said\n"), Event::End],
                &[Event::Open("agent main"), Event::End],
                &mut |_, _| {},
            );
            let told = given
                .recv_timeout(PATIENCE)
                .expect("the credit worker ends while its viewer still waits");
            drop(go);
            (told, viewing.join().unwrap())
        });
        let (failed, told) = told;
        assert!(failed.is_none(), "its credit worker ends as its capture does");
        assert_eq!(
            told.map(|e| e.status),
            Some(7),
            "the run's end it held is given back"
        );
        assert!(
            RunEnd(Arc::clone(&owner)).tell_after(ended(8)).is_some(),
            "an end told after its capture's goes at once"
        );
        assert_eq!(kept_in(&run), [b"said\n".to_vec(), Vec::new()]);
        assert_eq!(String::from_utf8(shown).unwrap(), "[agent main] said\n");
    }

    /// A capture whose window climbed to its most is credited within one endpoint record,
    /// and no channel of its run is aborted for it: the owner's credit worker, granting a
    /// subscribed channel, issues its credit at the window it climbed to, which its
    /// endpoint holds, and goes on; the run's other channel serves on (finding 1 of
    /// audit-fix-host-v1: the quantum climbed to `run::MAX_PAYLOAD`, past the endpoint's
    /// record, whose refusal ended the worker and every channel of the run with it).
    #[test]
    fn a_climbed_capture_credit_fits_one_endpoint_record_and_aborts_no_channel() {
        let server = identity::Identity::generate().unwrap();
        let owner = identity::Identity::generate().unwrap();
        let capacity = record_capacity(&["agent".into()], &[0]).unwrap();
        // The most one endpoint record holds, as its data frame carries it (its offset, then
        // the record, within one frame): checked against an endpoint of its own.
        let most = shards_remote::frame::MAX_FRAME - size_of::<u64>();
        {
            let (_, _endpoint, producer, _control) =
                bound(&server, owner.fingerprint(), Channel::Stdout, capacity);
            assert!(producer.admit(most).is_ok());
            assert!(producer.admit(most + 1).is_err());
        }
        let dir = shards_testdir::TempDir::new("managed-climb").unwrap();
        let metadata = shared_metadata(File::create(dir.join("metadata")).unwrap()).unwrap();
        let charge = Arc::new(Charge {
            budget: Arc::new(Mutex::new(Some(0))),
            bytes: Mutex::new(0),
        });
        let credits = Arc::new(Credits {
            state: Mutex::new(Queue {
                slots: vec![CreditSlot::Waiting, CreditSlot::Waiting],
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        });
        let (mut outputs, mut producers, mut endpoints) = (Vec::new(), Vec::new(), Vec::new());
        for (index, channel) in [Channel::Stdout, Channel::Stderr].into_iter().enumerate() {
            let (scope, endpoint, producer, control) = bound(&server, owner.fingerprint(), channel, capacity);
            let output = Arc::new(output(
                scope,
                &endpoint,
                control,
                capture_quantum(capacity).unwrap(),
                capacity,
                &metadata,
                Arc::downgrade(&credits),
                index,
                &charge,
            ));
            let weak = Arc::downgrade(&credits);
            producer.on_credit(move || {
                if let Some(credits) = weak.upgrade() {
                    credits.wake();
                }
            });
            outputs.push(Some(output));
            producers.push(Some(Arc::new(producer)));
            endpoints.push(endpoint);
        }
        let stdout = Arc::clone(outputs.first().unwrap().as_ref().unwrap());
        let stderr = Arc::clone(outputs.get(1).unwrap().as_ref().unwrap());
        let desired = climbed(&mut lock(&stdout.learning).quantum);
        #[cfg(unix)]
        let (stream, peer) = Stream::pair().unwrap();
        #[cfg(windows)]
        let (stream, peer) = Stream::plain_pair().unwrap();
        peer.set_read_timeout(Some(PATIENCE)).unwrap();
        let writing = stream.try_clone().unwrap();
        let owner_side = Owner {
            outputs,
            inputs: Vec::new(),
            drain: None,
            agent: None,
            stream,
            credits,
            charge: Arc::clone(&charge),
            ends: Arc::new(members::Ends::new(1).unwrap()),
            told: Mutex::new(Told::Capturing),
        };
        let (subscriber, sender) = shards_remote::net::Driver::bind_bounded(
            "127.0.0.1:0".parse().unwrap(),
            None,
            Subscriber {
                scope: stdout.scope,
                config: Some(
                    shards_remote::managed::client_config(&owner, server.fingerprint(), admitted(capacity))
                        .unwrap(),
                ),
                pending: Vec::new(),
                at: None,
                ended: false,
            },
            shards_remote::net::DriverLimits {
                connections: 1,
                command_slots: 1,
                queued_datagram_bytes: shards_remote::managed::DATAGRAM_BYTES,
            },
        )
        .unwrap();
        let (worked, work) = std::sync::mpsc::sync_channel(1);
        let (granted, ended, sibling) = std::thread::scope(|threads| {
            for endpoint in endpoints {
                threads.spawn(move || endpoint.run());
            }
            let mut subscriber = subscriber;
            threads.spawn(move || subscriber.run());
            sender
                .send(shards_remote::net::Command::Handler(stdout.address))
                .unwrap();
            let (owner_side, producers) = (&owner_side, &producers);
            threads.spawn(move || {
                // As the run's own credit worker ends (start_managed_capture): its owner, and
                // every channel of the run, with it.
                let result = send_credits(writing, owner_side, producers);
                if result.is_err() {
                    owner_side.abort();
                }
                let _ = worked.send(result);
            });
            let granted = credit(&peer);
            // A worker that sent a credit goes on; one that did not has ended, or ends.
            let ended = match granted {
                Ok(_) => work.try_recv().ok(),
                Err(_) => work.recv_timeout(PATIENCE).ok(),
            };
            let sibling = (*lock(&stderr.life), stderr.control.state());
            owner_side.abort();
            sender.request_stop();
            (granted, ended, sibling)
        });
        let record = std::fs::read_to_string(dir.join("metadata")).unwrap();
        assert!(
            ended.is_none(),
            "the credit worker ended at a capture's grant: {ended:?}; its run's record:\n{record}"
        );
        assert_eq!(
            sibling.0,
            Life::Serving,
            "the run's other channel ended for its sibling's credit: {sibling:?}"
        );
        assert!(!matches!(sibling.1, State::Aborted(_)), "{sibling:?}");
        let granted = granted.unwrap();
        assert_eq!((granted.member, granted.channel), (0, Channel::Stdout));
        assert!(
            desired <= most,
            "the capture's window climbed to {desired}, past the {most} one endpoint record holds"
        );
        assert!(desired > capacity, "the window never climbed");
        assert_eq!(
            granted.capacity, desired as u64,
            "the channel was credited short of the window it climbed to"
        );
        assert_eq!(lock(&stdout.learning).admitted, desired);
    }
}
