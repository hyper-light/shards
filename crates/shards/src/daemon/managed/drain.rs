//! What the daemon reads of a run's members itself (D240): each channel no owner reads
//! ([`super::OUTPUT_ANNOTATION`]) is credited again as soon as what came is kept, whole, in
//! its member's log, so that no member waits on a reader that may never come: a detached
//! run's, one whose client has gone, a terminal that stopped. A client attached to the run is
//! shown it as it is kept, each line after its member's label as D59 shows an agent's output,
//! on that client's stderr alone: the run's own stdout, stderr and log stay its command's.
//!
//! It keeps every member's log, owned or not (D240): the log is the run's. A drained channel's
//! records are kept here as they come; an owned channel's as its capture brings them, before
//! its owner is given them ([`Drain::keep`]).
//!
//! A member's log is `members/<index>` in its run's directory, its two channels as a run's log
//! keeps a command's stdout and stderr (spec.rs, `LOG_STDOUT`), as long and as many segments
//! as the run's own log keeps (segments.rs), its rotated segments compressed where the run's
//! own are (D162), its member's label beside it for `shards read logs` ([`member_logs`]); and
//! all of them with the command's within the run's one log cap (cap.rs). A run that keeps no
//! log (`--log-driver none`) keeps none of its members' either, and is shown nothing of them.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

use shards_abi::run::managed::{Channel, Event, Record};
use shards_vmm::platform;

use super::{Charge, cap, capture_quantum, route_index};
use crate::daemon::logs::{LogFile, Piece, Reader};
use crate::daemon::{lock, log, newest_log};
use crate::log_writer::Writer;
use crate::segments::new_segment;
use crate::spec::{LOG_STDERR, LOG_STDOUT, LogRetention};

/// Where the run's members' logs are in its directory.
const MEMBERS: &str = "members";
/// A member's label (`agent main`), kept beside its log for its readers (`shards read logs`).
const LABEL: &str = "label";

/// Where member `member`'s log is in its run's directory `run`.
pub(super) fn member_dir(run: &Path, member: u32) -> PathBuf {
    run.join(MEMBERS).join(member.to_string())
}

/// The members whose logs are in the run whose directory is `run`, by declared index: none
/// where it kept none.
pub(in crate::daemon) fn member_dirs(run: &Path) -> io::Result<Vec<(u32, PathBuf)>> {
    let mut members: Vec<u32> = match std::fs::read_dir(run.join(MEMBERS)) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    members.sort_unstable();
    let mut dirs = Vec::new();
    dirs.try_reserve_exact(members.len()).map_err(io::Error::other)?;
    dirs.extend(
        members
            .into_iter()
            .map(|member| (member, member_dir(run, member))),
    );
    Ok(dirs)
}

/// The logs of the run whose directory is `run` that its keeper keeps, owned or not, by its
/// members' declared index, each with its member's label: none where it kept none.
pub(in crate::daemon) fn member_logs(run: &Path) -> io::Result<Vec<(String, LogFile)>> {
    let members: Vec<u32> = member_dirs(run)?.into_iter().map(|(member, _)| member).collect();
    let mut logs = Vec::new();
    logs.try_reserve_exact(members.len()).map_err(io::Error::other)?;
    for member in members {
        let dir = member_dir(run, member);
        let label = match std::fs::read_to_string(dir.join(LABEL)) {
            Ok(label) => label,
            // A member's log without its label is none of its drain's.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        match LogFile::open(&dir) {
            Ok(log) => logs.push((label, log)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(logs)
}

/// How far a drained channel's stream has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Open,
    Ended,
    Aborted(i32),
}

struct Kept {
    /// Each kept member's log, by its declared index: none where the run keeps no log.
    logs: Vec<Option<Writer>>,
    /// Each channel's stream, by its credit route ([`route_index`]): none for one not kept.
    streams: Vec<Option<Stream>>,
    /// How often anything was kept or ended: what a viewer waits on to change.
    changes: u64,
    /// The run's capture has ended: nothing more comes.
    closed: bool,
    /// The run's one log cap, where it keeps its logs.
    cap: Option<Cap>,
    /// Members' segments rotated as they filled, to be compressed (D162): those since they were
    /// last taken ([`Drain::rotated`]), where the run's own log's are.
    rotated: Vec<(u32, u64)>,
}

impl Kept {
    /// Whether each of `members`' channels has ended, or the run's capture has.
    fn ended(&self, members: &[u32]) -> bool {
        self.closed
            || members.iter().all(|&member| {
                [Channel::Stdout, Channel::Stderr].into_iter().all(|channel| {
                    route_index(member, channel)
                        .ok()
                        .and_then(|route| self.streams.get(route).copied().flatten())
                        .is_none_or(|stream| stream != Stream::Open)
                })
            })
    }
}

/// How a run's logs are kept (D240): each member's as the run's own log is (`retention`), its
/// rotated segments compressed where the run's are (`compress`, D162), all of them within the
/// run's one cap.
#[derive(Debug, Clone, Copy)]
pub(super) struct Keeping {
    pub(super) retention: LogRetention,
    pub(super) compress: bool,
}

/// The run's one log cap as its keeper holds it (cap.rs): its bound, the output its logs held at
/// its last hold, and the output kept since. Its command's log grows unseen between its
/// rotations, each of which holds the cap again ([`Drain::cap`]): within one segment.
#[derive(Debug, Clone, Copy)]
struct Cap {
    /// The run's log bound: its segments' size times their count, as its own log keeps them.
    bound: u64,
    /// None until its first hold, which weighs what a run started again left.
    held: Option<u64>,
    since: u64,
}

pub(super) struct Drain {
    run: PathBuf,
    labels: Vec<String>,
    /// The members the daemon reads itself, ascending.
    members: Vec<u32>,
    /// The members whose logs it keeps, owned or not, ascending: none where the run keeps none.
    logged: Vec<u32>,
    /// Whether its members' rotated segments are compressed, as the run's own are (D162).
    compress: bool,
    /// The run's record capacity, the least window a channel is credited ([`Windows`]).
    capacity: usize,
    kept: Mutex<Kept>,
    changed: Condvar,
    /// Each drained or kept member's end, by its declared index: set once each of its channels
    /// has ended, or the run's capture has, all it wrote kept; what a log lane that follows its
    /// log waits on beside the log's own changes (D238).
    ends: Vec<Option<shards_ipc::Latch>>,
}

impl Drain {
    /// The keeper of the run whose directory is `run` and whose declared members are
    /// `labels`: draining `members`, the daemon's to read, and keeping their logs and those of
    /// `owned`, read by their owner, each begun where a run started again left it, as `keeping`
    /// says (none where the run keeps no log, and none of `owned` then).
    pub(super) fn open(
        run: &Path,
        labels: &[String],
        members: &[u32],
        owned: &[u32],
        keeping: Option<Keeping>,
        capacity: usize,
    ) -> io::Result<Drain> {
        let mut logged = Vec::new();
        if keeping.is_some() {
            logged
                .try_reserve_exact(members.len().saturating_add(owned.len()))
                .map_err(io::Error::other)?;
            logged.extend(members.iter().chain(owned).copied());
            logged.sort_unstable();
            logged.dedup();
        }
        let retention = keeping.map(|k| k.retention);
        let mut logs = Vec::new();
        logs.try_reserve_exact(labels.len()).map_err(io::Error::other)?;
        logs.resize_with(labels.len(), || None);
        let routes = labels
            .len()
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("member count overflow"))?;
        let mut streams = Vec::new();
        streams.try_reserve_exact(routes).map_err(io::Error::other)?;
        streams.resize(routes, None);
        // Every drained member's channels, and every kept one's.
        let mut tracked = Vec::new();
        tracked
            .try_reserve_exact(members.len().saturating_add(logged.len()))
            .map_err(io::Error::other)?;
        tracked.extend(members.iter().chain(&logged).copied());
        tracked.sort_unstable();
        tracked.dedup();
        for &member in &tracked {
            if let Some(retention) = retention.filter(|_| logged.contains(&member)) {
                let dir = member_dir(run, member);
                std::fs::create_dir_all(&dir)?;
                let label = usize::try_from(member)
                    .ok()
                    .and_then(|at| labels.get(at))
                    .ok_or_else(|| io::Error::other("kept member's label"))?;
                crate::os::owner_only(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true),
                )
                .open(dir.join(LABEL))?
                .write_all(label.as_bytes())?;
                let held = platform::open_dir(&dir)?;
                let newest = newest_log(&dir)?;
                let next = Box::new(move |seq| new_segment(&held, seq, retention.files));
                *logs
                    .get_mut(usize::try_from(member).map_err(io::Error::other)?)
                    .ok_or_else(|| io::Error::other("kept member index"))? = Some(Writer::new(
                    newest.log,
                    newest.index,
                    newest.seq,
                    retention,
                    next,
                )?);
            }
            for channel in [Channel::Stdout, Channel::Stderr] {
                *streams
                    .get_mut(route_index(member, channel)?)
                    .ok_or_else(|| io::Error::other("kept channel index"))? = Some(Stream::Open);
            }
        }
        let mut drained = Vec::new();
        drained
            .try_reserve_exact(members.len())
            .map_err(io::Error::other)?;
        drained.extend_from_slice(members);
        let mut names = Vec::new();
        names.try_reserve_exact(labels.len()).map_err(io::Error::other)?;
        for label in labels {
            names.push(label.clone());
        }
        let mut ends = Vec::new();
        ends.try_reserve_exact(labels.len()).map_err(io::Error::other)?;
        for member in 0..labels.len() {
            let kept = u32::try_from(member).is_ok_and(|m| tracked.contains(&m));
            ends.push(if kept {
                Some(shards_ipc::Latch::new()?)
            } else {
                None
            });
        }
        // The run's one bound: its own log's segments' size times their count (D186's history,
        // or its --log-opt max-size and max-file), shared by its command's log and its members'.
        let cap = retention.map(|r| Cap {
            bound: r.size.saturating_mul(r.files),
            held: None,
            since: 0,
        });
        Ok(Drain {
            run: run.to_path_buf(),
            labels: names,
            members: drained,
            logged,
            compress: keeping.is_some_and(|k| k.compress),
            capacity,
            kept: Mutex::new(Kept {
                logs,
                streams,
                changes: 0,
                closed: false,
                cap,
                rotated: Vec::new(),
            }),
            changed: Condvar::new(),
            ends,
        })
    }

    /// Member `member`'s end ([`Drain::ends`]), another handle to it: none for a member it
    /// neither drains nor keeps.
    pub(super) fn end_of(&self, member: u32) -> Option<io::Result<shards_ipc::Latch>> {
        let at = usize::try_from(member).ok()?;
        self.ends.get(at)?.as_ref().map(shards_ipc::Latch::try_clone)
    }

    /// Sets the end of each drained or kept member whose channels have all ended, or of every
    /// one once the run's capture has.
    fn tell_ends(&self, kept: &Kept) {
        for (member, end) in self.ends.iter().enumerate() {
            let Some(end) = end else {
                continue;
            };
            let open = u32::try_from(member).is_ok_and(|member| {
                [Channel::Stdout, Channel::Stderr].into_iter().any(|channel| {
                    route_index(member, channel)
                        .ok()
                        .and_then(|route| kept.streams.get(route).copied().flatten())
                        .is_some_and(|stream| stream == Stream::Open)
                })
            });
            if kept.closed || !open {
                end.set();
            }
        }
    }

    /// Whether the daemon reads credit route `index` itself.
    pub(super) fn drains(&self, index: usize) -> bool {
        u32::try_from(index / 2).is_ok_and(|member| self.drains_member(member))
    }

    /// Whether the daemon reads member `member`'s output itself.
    pub(super) fn drains_member(&self, member: u32) -> bool {
        self.members.contains(&member)
    }

    /// Whether credit route `index`'s records come to it: a drained channel's, or a kept one's.
    pub(super) fn keeps(&self, index: usize) -> bool {
        lock(&self.kept).streams.get(index).is_some_and(Option::is_some)
    }

    /// Whether member `member`'s records come to it: a drained member's, or a kept one's.
    pub(super) fn keeps_member(&self, member: u32) -> bool {
        [Channel::Stdout, Channel::Stderr]
            .into_iter()
            .any(|channel| route_index(member, channel).is_ok_and(|route| self.keeps(route)))
    }

    /// The members the daemon reads itself, ascending.
    pub(super) fn members(&self) -> &[u32] {
        &self.members
    }

    /// Keeps `record`, a drained or kept channel's, as its VM sent it: its data in its member's
    /// log, its end as its stream's. Whether its stream has ended with it. One that comes after
    /// its stream ended (its member aborted under it) is nothing more. Where what its logs hold
    /// may pass the run's bound, its cap holds again.
    pub(super) fn keep(&self, record: &[u8]) -> io::Result<bool> {
        let record =
            Record::decode(record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let route = route_index(record.member, record.channel)?;
        let mut kept = lock(&self.kept);
        let stream =
            kept.streams.get(route).copied().flatten().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "a record of a channel not kept")
            })?;
        if stream != Stream::Open {
            return Ok(true);
        }
        let end = match record.event {
            Event::Open(_) => None,
            Event::Data(bytes) => {
                let at = usize::try_from(record.member).map_err(io::Error::other)?;
                let Kept {
                    logs, cap, rotated, ..
                } = &mut *kept;
                if let Some(Some(writer)) = logs.get_mut(at) {
                    let (seq, written) = (writer.seq(), writer.written());
                    writer.keep(log_stream(record.channel), bytes);
                    // Each segment it filled is rotated: compressed where the run's are.
                    if self.compress {
                        let filled = writer.seq().saturating_sub(seq);
                        rotated
                            .try_reserve(usize::try_from(filled).map_err(io::Error::other)?)
                            .map_err(io::Error::other)?;
                        rotated.extend((seq..writer.seq()).map(|seq| (record.member, seq)));
                    }
                    if let Some(cap) = cap {
                        cap.since = cap.since.saturating_add(writer.written().saturating_sub(written));
                    }
                }
                if kept.cap.is_some_and(|cap| {
                    cap.held
                        .is_none_or(|held| held.saturating_add(cap.since) > cap.bound)
                }) {
                    self.hold(&mut kept);
                }
                None
            }
            Event::End => Some(Stream::Ended),
            Event::Aborted(errno) => Some(Stream::Aborted(errno)),
        };
        if let (Some(end), Some(slot)) = (end, kept.streams.get_mut(route)) {
            *slot = Some(end);
            self.tell_ends(&kept);
        }
        kept.changes = kept.changes.wrapping_add(1);
        drop(kept);
        self.changed.notify_all();
        Ok(end.is_some())
    }

    /// The run's command's log has rotated (files.rs): its one cap holds again, the segment its
    /// VM finished now weighed.
    pub(super) fn cap(&self) {
        let mut kept = lock(&self.kept);
        if kept.cap.is_some() {
            self.hold(&mut kept);
        }
    }

    /// Holds the run's logs within its one bound (cap.rs), closing a member's newest through its
    /// writer where every log is down to its newest. One that fails is said in the daemon's log,
    /// and holds again once a bound's worth more is kept: the logs keep their output meanwhile.
    fn hold(&self, kept: &mut Kept) {
        let Kept { logs, cap, .. } = kept;
        let Some(limit) = cap else {
            return;
        };
        let held = cap::hold(&self.run, &self.logged, limit.bound, &mut |member| {
            usize::try_from(member)
                .ok()
                .and_then(|at| logs.get_mut(at))
                .and_then(Option::as_mut)
                .ok_or_else(|| io::Error::other("a member's log without its writer"))?
                .rotate()
        });
        limit.since = 0;
        limit.held = Some(match held {
            Ok(held) => held.total,
            Err(e) => {
                log(format!(
                    "{}: holding its logs to its bound: {e}",
                    self.run.display()
                ));
                0
            }
        });
    }

    /// The members' segments rotated as they filled since last asked, each its log's directory
    /// and number: for the daemon to compress, as the run's own (D162). None where the run's are
    /// not compressed.
    pub(super) fn rotated(&self) -> Vec<(PathBuf, u64)> {
        let taken = std::mem::take(&mut lock(&self.kept).rotated);
        taken
            .into_iter()
            .map(|(member, seq)| (member_dir(&self.run, member), seq))
            .collect()
    }

    /// Member `member` aborted (`IPC86`, its init's word): both its channels end with
    /// `errno`, and what comes of them after is nothing more.
    pub(super) fn abort_member(&self, member: u32, errno: i32) {
        let mut kept = lock(&self.kept);
        for channel in [Channel::Stdout, Channel::Stderr] {
            if let Ok(route) = route_index(member, channel)
                && let Some(Some(stream @ Stream::Open)) = kept.streams.get_mut(route)
            {
                *stream = Stream::Aborted(errno);
            }
        }
        self.tell_ends(&kept);
        kept.changes = kept.changes.wrapping_add(1);
        drop(kept);
        self.changed.notify_all();
    }

    /// The run's capture has ended: nothing more comes. What a log could not keep (a full
    /// disk) is said in the daemon's log, as the run's own log says its losses.
    pub(super) fn close(&self) {
        let mut kept = lock(&self.kept);
        if kept.closed {
            return;
        }
        kept.closed = true;
        self.tell_ends(&kept);
        for (member, writer) in kept.logs.iter().enumerate() {
            if let Some(writer) = writer
                && writer.lost() > 0
            {
                log(format!(
                    "{}: {} bytes of its {} output its log could not keep",
                    self.run.display(),
                    writer.lost(),
                    self.labels.get(member).map_or("member", String::as_str)
                ));
            }
        }
        drop(kept);
        self.changed.notify_all();
    }

    /// Shows what is kept, as it is kept, on `to`, an attached client's stderr: each line of
    /// each drained member's channels after its member's label, until every channel has
    /// ended or the run's capture has; at once where `to` takes no more, its client gone, as
    /// nothing waits on what it shows.
    pub(super) fn show(&self, to: &mut dyn Write) {
        let mut shown: Vec<Viewed> = self
            .members
            .iter()
            .map(|&member| Viewed {
                dir: member_dir(&self.run, member),
                label: usize::try_from(member)
                    .ok()
                    .and_then(|at| self.labels.get(at))
                    .cloned()
                    .unwrap_or_default(),
                log: None,
                reader: Reader::new(),
            })
            .collect();
        let mut seen = None;
        // Each line goes in one write, so that what else writes to the client's stderr (its
        // command's own) comes between its lines, never within one.
        let mut whole = Vec::new();
        loop {
            let ended = {
                let mut kept = lock(&self.kept);
                while seen == Some(kept.changes) && !kept.ended(&self.members) {
                    kept = self.changed.wait(kept).unwrap_or_else(PoisonError::into_inner);
                }
                seen = Some(kept.changes);
                kept.ended(&self.members)
            };
            for member in &mut shown {
                if !member.read(to, &mut whole) {
                    return;
                }
            }
            if ended {
                for member in &mut shown {
                    let Viewed { label, reader, .. } = member;
                    let mut each = |piece: Piece<'_>| line(to, &mut whole, label, &piece);
                    if !matches!(reader.finish(&mut each), Ok(true)) {
                        return;
                    }
                }
                return;
            }
        }
    }
}

/// A member as a viewer reads it: its log, once it is there, and where it has read to.
struct Viewed {
    dir: PathBuf,
    label: String,
    log: Option<LogFile>,
    reader: Reader,
}

impl Viewed {
    /// Shows what its log has whole since it last looked, each line made in `whole`. Whether
    /// `to` took it all.
    fn read(&mut self, to: &mut dyn Write, whole: &mut Vec<u8>) -> bool {
        if self.log.is_none() {
            self.log = LogFile::open(&self.dir).ok();
        }
        let Viewed {
            label, log, reader, ..
        } = self;
        match log {
            Some(log) => matches!(
                reader.read(log, &mut |piece| line(to, whole, label, &piece)),
                Ok(true)
            ),
            None => true,
        }
    }
}

/// Writes `piece` of a member's output as a line of its own after `[label] `, as D59 shows an
/// agent's, made whole in `whole` and written at once: a line longer than a piece goes in
/// pieces, each a line, and one cut short at its stream's end is ended. Whether to go on: not
/// once `to` refuses it.
fn line(to: &mut dyn Write, whole: &mut Vec<u8>, label: &str, piece: &Piece<'_>) -> io::Result<bool> {
    whole.clear();
    let len = label
        .len()
        .saturating_add(piece.bytes.len())
        .saturating_add(b"[] \n".len());
    // A line with no room to be made ends what is shown; nothing waits on it.
    if whole.try_reserve(len).is_err() {
        return Ok(false);
    }
    whole.push(b'[');
    whole.extend_from_slice(label.as_bytes());
    whole.extend_from_slice(b"] ");
    whole.extend_from_slice(piece.bytes);
    if piece.bytes.last() != Some(&b'\n') {
        whole.push(b'\n');
    }
    Ok(to.write_all(whole).is_ok())
}

/// The run log's stream byte a channel's output is kept as.
fn log_stream(channel: Channel) -> u8 {
    match channel {
        Channel::Stdout => LOG_STDOUT,
        Channel::Stderr => LOG_STDERR,
    }
}

/// What the daemon has learned of each drained channel's credit, as an owner's is learned
/// (D226): its window, which climbs in the run's record capacity as the drain keeps whole
/// records as fast as they come, and the buffer it lends the next record.
pub(super) struct Windows {
    channels: Vec<Option<Window>>,
}

struct Window {
    quantum: shards_registry::Quantum,
    /// The window granted and charged.
    admitted: usize,
    /// When the record in flight was granted, and its window.
    granted: Option<(Instant, usize)>,
    /// The buffer lent the next record.
    buffer: Option<Vec<u8>>,
}

impl Windows {
    pub(super) fn new(drain: Option<&Drain>) -> io::Result<Windows> {
        let Some(drain) = drain else {
            return Ok(Windows { channels: Vec::new() });
        };
        let routes = lock(&drain.kept).streams.len();
        let mut channels = Vec::new();
        channels.try_reserve_exact(routes).map_err(io::Error::other)?;
        for index in 0..routes {
            channels.push(if drain.drains(index) {
                Some(Window {
                    quantum: capture_quantum(drain.capacity)?,
                    admitted: drain.capacity,
                    granted: None,
                    buffer: None,
                })
            } else {
                None
            });
        }
        Ok(Windows { channels })
    }

    pub(super) fn covers(&self, index: usize) -> bool {
        self.channels.get(index).is_some_and(Option::is_some)
    }

    fn window(&mut self, index: usize) -> io::Result<&mut Window> {
        self.channels
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| io::Error::other("a credit route not drained"))
    }

    /// The credit to grant channel `index`: the window it has learned, grown where its
    /// quantum climbed and the host holds the growth (`charge`), and a buffer that holds it.
    pub(super) fn grant(&mut self, index: usize, charge: &Charge) -> io::Result<(usize, Vec<u8>)> {
        let window = self.window(index)?;
        let desired = window.quantum.desired();
        if desired > window.admitted {
            let extra = u64::try_from(desired - window.admitted).map_err(io::Error::other)?;
            // A window the host cannot hold now leaves the channel the one it has.
            if charge.grow(extra).is_ok() {
                window.admitted = desired;
                window.buffer = None;
            }
        }
        let mut buffer = match window.buffer.take() {
            Some(buffer) if buffer.capacity() >= window.admitted => buffer,
            _ => {
                let mut fresh = Vec::new();
                fresh
                    .try_reserve_exact(window.admitted)
                    .map_err(io::Error::other)?;
                fresh
            }
        };
        buffer.clear();
        let capacity = window.admitted;
        window.granted = Some((Instant::now(), capacity));
        Ok((capacity, buffer))
    }

    /// Takes back a buffer granted no record.
    pub(super) fn lend(&mut self, index: usize, buffer: Vec<u8>) {
        if let Ok(window) = self.window(index) {
            window.granted = None;
            window.buffer = Some(buffer);
        }
    }

    /// Channel `index`'s record `bytes` is kept: a whole window's is a sample of how fast the
    /// drain keeps what comes (D226's "full data record consumed"), and its buffer is lent to
    /// the next.
    pub(super) fn kept(&mut self, index: usize, bytes: Vec<u8>) {
        if let Ok(window) = self.window(index) {
            if let Some((began, granted)) = window.granted.take()
                && bytes.len() == granted
            {
                let _ = window.quantum.consumed(began, Instant::now(), bytes.len() as u64);
            }
            window.buffer = Some(bytes);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use shards_abi::run::managed::Record;

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

    fn labels() -> Vec<String> {
        vec!["agent files".into(), "agent main".into(), "harness drive".into()]
    }

    const KEEP: LogRetention = LogRetention {
        size: 1 << 20,
        files: 2,
    };
    const KEEPING: Keeping = Keeping {
        retention: KEEP,
        compress: false,
    };

    /// A drained member's records are kept in its log, each channel apart, its bytes whole;
    /// its end ends its stream, and what comes after is nothing more; a member an owner reads
    /// is no drained one's.
    #[test]
    fn a_drained_members_output_is_kept_whole_in_its_log() {
        let run = shards_testdir::TempDir::new("drain-keep").unwrap();
        let drain = Drain::open(&run, &labels(), &[1], &[], Some(KEEPING), 64).unwrap();
        assert!(drain.drains(route_index(1, Channel::Stdout).unwrap()));
        assert!(!drain.drains(route_index(2, Channel::Stdout).unwrap()));
        assert!(drain.drains_member(1) && !drain.drains_member(2));
        let out = [b'a', 0, 0xff, b'\n'];
        assert!(
            !drain
                .keep(&encoded(1, Channel::Stdout, 0, Event::Open("agent main")))
                .unwrap()
        );
        assert!(
            !drain
                .keep(&encoded(1, Channel::Stdout, 0, Event::Data(&out)))
                .unwrap()
        );
        assert!(
            !drain
                .keep(&encoded(1, Channel::Stderr, 0, Event::Data(b"oops")))
                .unwrap()
        );
        assert!(drain.keep(&encoded(1, Channel::Stdout, 4, Event::End)).unwrap());
        assert!(
            drain
                .keep(&encoded(1, Channel::Stdout, 4, Event::Data(b"late")))
                .unwrap()
        );
        assert!(
            drain
                .keep(&encoded(2, Channel::Stdout, 0, Event::Data(b"x")))
                .is_err()
        );
        drain.close();
        let log = LogFile::open(&member_dir(&run, 1)).unwrap();
        let mut reader = Reader::new();
        let mut got = Vec::new();
        let mut each = |p: Piece<'_>| {
            got.push((p.stream, p.bytes.to_vec()));
            Ok(true)
        };
        reader.read(&log, &mut each).unwrap();
        reader.finish(&mut each).unwrap();
        assert_eq!(got, [(LOG_STDOUT, out.to_vec()), (LOG_STDERR, b"oops".to_vec())]);
    }

    /// A viewer shows each line of each drained member after its label, a line cut short
    /// ended, and returns once every channel has ended.
    #[test]
    fn a_viewer_shows_each_line_after_its_member() {
        let run = shards_testdir::TempDir::new("drain-show").unwrap();
        let drain = Drain::open(&run, &labels(), &[0, 1], &[], Some(KEEPING), 64).unwrap();
        let mut shown = Vec::new();
        std::thread::scope(|threads| {
            let viewer = threads.spawn(|| {
                let mut to = Vec::new();
                drain.show(&mut to);
                to
            });
            drain
                .keep(&encoded(
                    1,
                    Channel::Stdout,
                    0,
                    Event::Data(b"confined uid=200001\nhalf"),
                ))
                .unwrap();
            drain
                .keep(&encoded(0, Channel::Stderr, 0, Event::Data(b"x\n")))
                .unwrap();
            for member in [0, 1] {
                for channel in [Channel::Stdout, Channel::Stderr] {
                    drain.keep(&encoded(member, channel, 0, Event::End)).unwrap();
                }
            }
            shown = viewer.join().unwrap();
        });
        let shown = String::from_utf8(shown).unwrap();
        let mut lines: Vec<&str> = shown.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "[agent files] x",
                "[agent main] confined uid=200001",
                "[agent main] half"
            ]
        );
    }

    /// Each line a viewer shows goes in one write, so that what else writes to its client's
    /// stderr (its command's own output) comes between its lines, never within one.
    #[test]
    fn a_viewer_writes_each_line_at_once() {
        struct Writes(Vec<Vec<u8>>);
        impl Write for Writes {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.push(bytes.to_vec());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let run = shards_testdir::TempDir::new("drain-whole").unwrap();
        let drain = Drain::open(&run, &labels(), &[1], &[], Some(KEEPING), 64).unwrap();
        drain
            .keep(&encoded(1, Channel::Stdout, 0, Event::Data(b"one\ntwo\nthr")))
            .unwrap();
        drain
            .keep(&encoded(1, Channel::Stderr, 0, Event::Data(b"err\n")))
            .unwrap();
        drain.keep(&encoded(1, Channel::Stdout, 11, Event::End)).unwrap();
        drain.keep(&encoded(1, Channel::Stderr, 4, Event::End)).unwrap();
        let mut to = Writes(Vec::new());
        drain.show(&mut to);
        assert_eq!(
            to.0,
            [
                &b"[agent main] one\n"[..],
                b"[agent main] two\n",
                b"[agent main] err\n",
                b"[agent main] thr\n"
            ]
        );
    }

    /// A run's members' logs are found with their labels, by declared index, as `logs`
    /// reads them; a run that drains none has none, and nor has one that keeps no log.
    #[test]
    fn a_runs_member_logs_are_found_with_their_labels() {
        let run = shards_testdir::TempDir::new("drain-logs").unwrap();
        assert!(member_logs(&run).unwrap().is_empty());
        let drain = Drain::open(&run, &labels(), &[0, 2], &[], Some(KEEPING), 64).unwrap();
        drain
            .keep(&encoded(2, Channel::Stdout, 0, Event::Data(b"x\n")))
            .unwrap();
        drain.close();
        let found = member_logs(&run).unwrap();
        let named: Vec<&str> = found.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(named, ["agent files", "harness drive"]);
        let mut got = Vec::new();
        Reader::new()
            .read(&found[1].1, &mut |p| {
                got.extend_from_slice(p.bytes);
                Ok(true)
            })
            .unwrap();
        assert_eq!(got, b"x\n");
        let none = shards_testdir::TempDir::new("drain-logs-none").unwrap();
        let _ = Drain::open(&none, &labels(), &[1], &[], None, 64).unwrap();
        assert!(member_logs(&none).unwrap().is_empty());
    }

    /// A viewer whose client has gone stops at once; the drain keeps going.
    #[test]
    fn a_viewer_whose_client_has_gone_stops() {
        struct Gone;
        impl Write for Gone {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let run = shards_testdir::TempDir::new("drain-gone").unwrap();
        let drain = Drain::open(&run, &labels(), &[1], &[], Some(KEEPING), 64).unwrap();
        drain
            .keep(&encoded(1, Channel::Stdout, 0, Event::Data(b"a\n")))
            .unwrap();
        // It returns though no channel has ended.
        drain.show(&mut Gone);
        assert!(
            !drain
                .keep(&encoded(1, Channel::Stdout, 2, Event::Data(b"b\n")))
                .unwrap()
        );
    }

    /// A run that keeps no log keeps none of its members': their records are taken all the
    /// same, so no member waits.
    #[test]
    fn a_run_that_keeps_no_log_keeps_none_of_its_members() {
        let run = shards_testdir::TempDir::new("drain-none").unwrap();
        let drain = Drain::open(&run, &labels(), &[1], &[], None, 64).unwrap();
        assert!(
            !drain
                .keep(&encoded(1, Channel::Stdout, 0, Event::Data(b"a\n")))
                .unwrap()
        );
        assert!(!member_dir(&run, 1).exists());
    }

    /// The segments the log in `dir` keeps, oldest first.
    fn segments(dir: &Path) -> Vec<u64> {
        let mut seqs: Vec<u64> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| crate::daemon::logs::segment_of(e.unwrap().file_name().to_str()?))
            .collect();
        seqs.sort_unstable();
        seqs
    }

    /// Ages the index of segment `seq` of the log in `dir` to `secs` past the epoch: what the
    /// run's cap orders its logs' oldest by.
    fn age(dir: &Path, seq: u64, secs: u64) {
        std::fs::File::options()
            .write(true)
            .open(dir.join(crate::segments::log_segment(seq).1))
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    /// Everything the log in `dir` keeps, its output's bytes in order.
    fn read_whole(dir: &Path) -> Vec<u8> {
        let mut got = Vec::new();
        let mut reader = Reader::new();
        let mut each = |p: Piece<'_>| {
            got.extend_from_slice(p.bytes);
            Ok(true)
        };
        reader.read(&LogFile::open(dir).unwrap(), &mut each).unwrap();
        reader.finish(&mut each).unwrap();
        got
    }

    /// A member's log is kept, owned or not, and counted in its run's one cap: an owned member's
    /// records, which its capture brings past the drain to its owner, are kept in its log, found
    /// with its label as `logs` finds a drained member's, and their bytes, past the run's bound
    /// with its command's, let the run's oldest segment go, its command's, recorded as its loss.
    #[test]
    fn a_members_log_is_kept_and_counted_in_its_runs_cap() {
        let run = shards_testdir::TempDir::new("keep-owned").unwrap();
        // The command's log as its VM writes it: records of 17 bytes in segments of 30, one a
        // segment, its first two aged oldest.
        let held = platform::open_dir(&run).unwrap();
        let newest = newest_log(&run).unwrap();
        let mut command = Writer::new(
            newest.log,
            newest.index,
            newest.seq,
            LogRetention { size: 30, files: 3 },
            Box::new(move |seq| new_segment(&held, seq, 3)),
        )
        .unwrap();
        for _ in 0..3 {
            command.keep(LOG_STDOUT, b"cmd\n");
        }
        age(&run, 0, 10);
        age(&run, 1, 20);
        // Its bound, 90 bytes: three segments of 30.
        let keeping = Keeping {
            retention: LogRetention { size: 30, files: 3 },
            compress: false,
        };
        let drain = Drain::open(&run, &labels(), &[], &[1], Some(keeping), 64).unwrap();
        let route = route_index(1, Channel::Stdout).unwrap();
        assert!(
            !drain.drains(route) && drain.keeps(route),
            "an owned channel is kept, not drained"
        );
        for _ in 0..3 {
            drain
                .keep(&encoded(1, Channel::Stdout, 0, Event::Data(b"mbr\n")))
                .unwrap();
        }
        let found = member_logs(&run).unwrap();
        let named: Vec<&str> = found.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(named, ["agent main"]);
        assert_eq!(read_whole(&member_dir(&run, 1)), b"mbr\nmbr\nmbr\n");
        // 102 bytes past 90: the command's oldest goes, its loss its own record's.
        assert_eq!(segments(&run), [1, 2]);
        assert_eq!(segments(&member_dir(&run, 1)), [0, 1, 2]);
        let capped = |dir: &Path| cap::capped(&platform::open_dir(dir).unwrap()).unwrap();
        assert_eq!(
            capped(&run),
            cap::Capped {
                segments: 1,
                bytes: 17
            }
        );
        assert_eq!(capped(&member_dir(&run, 1)), cap::Capped::default());
    }

    /// A member's rotated segment is compressed when the run's options say so, as the run's own
    /// are (D162): each segment its log filled is handed to the daemon's packer, which leaves it
    /// read back whole; and none where the run's are not compressed.
    #[test]
    fn a_members_rotated_segment_is_compressed_when_the_runs_options_say_so() {
        let retention = LogRetention { size: 30, files: 8 };
        for compress in [true, false] {
            let run = shards_testdir::TempDir::new("keep-packed").unwrap();
            let keeping = Keeping { retention, compress };
            let drain = Drain::open(&run, &labels(), &[1], &[2], Some(keeping), 64).unwrap();
            for member in [1, 2] {
                for _ in 0..3 {
                    drain
                        .keep(&encoded(member, Channel::Stdout, 0, Event::Data(b"out\n")))
                        .unwrap();
                }
            }
            let mut rotated = drain.rotated();
            rotated.sort_unstable();
            if !compress {
                assert!(rotated.is_empty(), "{rotated:?}");
                continue;
            }
            let (one, two) = (member_dir(&run, 1), member_dir(&run, 2));
            assert_eq!(
                rotated,
                [
                    (one.clone(), 0),
                    (one.clone(), 1),
                    (two.clone(), 0),
                    (two.clone(), 1)
                ]
            );
            assert!(drain.rotated().is_empty(), "each handed once");
            for (dir, seq) in &rotated {
                assert!(
                    crate::daemon::packed::pack_for_test(&platform::open_dir(dir).unwrap(), *seq).unwrap()
                );
            }
            for dir in [&one, &two] {
                let names: Vec<String> = std::fs::read_dir(dir)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect();
                for packed in ["log.zst", "log.1.zst"] {
                    assert!(names.iter().any(|n| n == packed), "{names:?}");
                }
                assert!(!names.iter().any(|n| n == "log" || n == "log.1"), "{names:?}");
                assert_eq!(read_whole(dir), b"out\nout\nout\n");
            }
        }
    }

    /// A run started again goes on in its members' newest segment.
    #[test]
    fn a_run_started_again_goes_on_in_its_members_logs() {
        let run = shards_testdir::TempDir::new("drain-again").unwrap();
        for word in [&b"first\n"[..], b"second\n"] {
            let drain = Drain::open(&run, &labels(), &[1], &[], Some(KEEPING), 64).unwrap();
            drain
                .keep(&encoded(1, Channel::Stdout, 0, Event::Data(word)))
                .unwrap();
            drain.close();
        }
        let log = LogFile::open(&member_dir(&run, 1)).unwrap();
        let mut reader = Reader::new();
        let mut got = Vec::new();
        reader
            .read(&log, &mut |p| {
                got.extend_from_slice(p.bytes);
                Ok(true)
            })
            .unwrap();
        assert_eq!(got, b"first\nsecond\n");
    }
}
