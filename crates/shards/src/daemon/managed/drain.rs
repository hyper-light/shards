//! What the daemon reads of a run's members itself (D240): each channel no owner reads
//! ([`super::OUTPUT_ANNOTATION`]) is credited again as soon as what came is kept, whole, in
//! its member's log, so that no member waits on a reader that may never come: a detached
//! run's, one whose client has gone, a terminal that stopped. A client attached to the run is
//! shown it as it is kept, each line after its member's label as D59 shows an agent's output,
//! on that client's stderr alone: the run's own stdout, stderr and log stay its command's.
//!
//! A member's log is `members/<index>` in its run's directory, its two channels as a run's log
//! keeps a command's stdout and stderr (spec.rs, `LOG_STDOUT`), as long and as many segments
//! as the run's own log keeps (segments.rs); a run that keeps no log (`--log-driver none`)
//! keeps none of its members' either, and is shown nothing of them.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

use shards_abi::run::managed::{Channel, Event, Record};
use shards_vmm::platform;

use super::{Charge, capture_quantum, route_index};
use crate::daemon::logs::{LogFile, Piece, Reader};
use crate::daemon::{lock, log, newest_log};
use crate::log_writer::Writer;
use crate::segments::new_segment;
use crate::spec::{LOG_STDERR, LOG_STDOUT, LogRetention};

/// Where member `member`'s log is in its run's directory `run`.
pub(super) fn member_dir(run: &Path, member: u32) -> PathBuf {
    run.join("members").join(member.to_string())
}

/// How far a drained channel's stream has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Open,
    Ended,
    Aborted(i32),
}

struct Kept {
    /// Each drained member's log, by its declared index: none for one an owner reads, or
    /// where the run keeps no log.
    logs: Vec<Option<Writer>>,
    /// Each channel's stream, by its credit route ([`route_index`]): none for one not drained.
    streams: Vec<Option<Stream>>,
    /// How often anything was kept or ended: what a viewer waits on to change.
    changes: u64,
    /// The run's capture has ended: nothing more comes.
    closed: bool,
}

impl Kept {
    fn ended(&self) -> bool {
        self.closed || self.streams.iter().flatten().all(|s| *s != Stream::Open)
    }
}

pub(super) struct Drain {
    run: PathBuf,
    labels: Vec<String>,
    members: Vec<u32>,
    /// The run's record capacity, the least window a channel is credited ([`Windows`]).
    capacity: usize,
    kept: Mutex<Kept>,
    changed: Condvar,
}

impl Drain {
    /// The drain of `members` of the run whose directory is `run` and whose declared members
    /// are `labels`: each member's log begun where a run started again left it, kept as
    /// `retention` keeps the run's own (none where it keeps none).
    pub(super) fn open(
        run: &Path,
        labels: &[String],
        members: &[u32],
        retention: Option<LogRetention>,
        capacity: usize,
    ) -> io::Result<Drain> {
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
        for &member in members {
            if let Some(retention) = retention {
                let dir = member_dir(run, member);
                std::fs::create_dir_all(&dir)?;
                let held = platform::open_dir(&dir)?;
                let newest = newest_log(&dir)?;
                let next = Box::new(move |seq| new_segment(&held, seq, retention.files));
                *logs
                    .get_mut(usize::try_from(member).map_err(io::Error::other)?)
                    .ok_or_else(|| io::Error::other("drained member index"))? = Some(Writer::new(
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
                    .ok_or_else(|| io::Error::other("drained channel index"))? = Some(Stream::Open);
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
        Ok(Drain {
            run: run.to_path_buf(),
            labels: names,
            members: drained,
            capacity,
            kept: Mutex::new(Kept {
                logs,
                streams,
                changes: 0,
                closed: false,
            }),
            changed: Condvar::new(),
        })
    }

    /// Whether the daemon reads credit route `index` itself.
    pub(super) fn drains(&self, index: usize) -> bool {
        lock(&self.kept).streams.get(index).is_some_and(Option::is_some)
    }

    /// Whether the daemon reads member `member`'s output itself.
    pub(super) fn drains_member(&self, member: u32) -> bool {
        self.members.contains(&member)
    }

    /// Keeps `record`, a drained channel's, as its VM sent it: its data in its member's log,
    /// its end as its stream's. Whether its stream has ended with it. One that comes after its
    /// stream ended (its member aborted under it) is nothing more.
    pub(super) fn keep(&self, record: &[u8]) -> io::Result<bool> {
        let record =
            Record::decode(record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let route = route_index(record.member, record.channel)?;
        let mut kept = lock(&self.kept);
        let stream =
            kept.streams.get(route).copied().flatten().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "a record of a channel not drained")
            })?;
        if stream != Stream::Open {
            return Ok(true);
        }
        let end = match record.event {
            Event::Open(_) => None,
            Event::Data(bytes) => {
                let at = usize::try_from(record.member).map_err(io::Error::other)?;
                if let Some(Some(writer)) = kept.logs.get_mut(at) {
                    writer.keep(log_stream(record.channel), bytes);
                }
                None
            }
            Event::End => Some(Stream::Ended),
            Event::Aborted(errno) => Some(Stream::Aborted(errno)),
        };
        if let (Some(end), Some(slot)) = (end, kept.streams.get_mut(route)) {
            *slot = Some(end);
        }
        kept.changes = kept.changes.wrapping_add(1);
        drop(kept);
        self.changed.notify_all();
        Ok(end.is_some())
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
        loop {
            let ended = {
                let mut kept = lock(&self.kept);
                while seen == Some(kept.changes) && !kept.ended() {
                    kept = self.changed.wait(kept).unwrap_or_else(PoisonError::into_inner);
                }
                seen = Some(kept.changes);
                kept.ended()
            };
            for member in &mut shown {
                if !member.read(to) {
                    return;
                }
            }
            if ended {
                for member in &mut shown {
                    let Viewed { label, reader, .. } = member;
                    if !matches!(reader.finish(&mut |piece| line(to, label, &piece)), Ok(true)) {
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
    /// Shows what its log has whole since it last looked. Whether `to` took it all.
    fn read(&mut self, to: &mut dyn Write) -> bool {
        if self.log.is_none() {
            self.log = LogFile::open(&self.dir).ok();
        }
        let Viewed {
            label, log, reader, ..
        } = self;
        match log {
            Some(log) => matches!(reader.read(log, &mut |piece| line(to, label, &piece)), Ok(true)),
            None => true,
        }
    }
}

/// Writes `piece` of a member's output as a line of its own after `[label] `, as D59 shows an
/// agent's: a line longer than a piece goes in pieces, each a line, and one cut short at
/// its stream's end is ended. Whether to go on: not once `to` refuses it.
fn line(to: &mut dyn Write, label: &str, piece: &Piece<'_>) -> io::Result<bool> {
    let newline: &[u8] = if piece.bytes.last() == Some(&b'\n') {
        b""
    } else {
        b"\n"
    };
    let written = to
        .write_all(b"[")
        .and_then(|()| to.write_all(label.as_bytes()))
        .and_then(|()| to.write_all(b"] "))
        .and_then(|()| to.write_all(piece.bytes))
        .and_then(|()| to.write_all(newline));
    Ok(written.is_ok())
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

    /// A drained member's records are kept in its log, each channel apart, its bytes whole;
    /// its end ends its stream, and what comes after is nothing more; a member an owner reads
    /// is no drained one's.
    #[test]
    fn a_drained_members_output_is_kept_whole_in_its_log() {
        let run = shards_testdir::TempDir::new("drain-keep").unwrap();
        let drain = Drain::open(&run, &labels(), &[1], Some(KEEP), 64).unwrap();
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
        let drain = Drain::open(&run, &labels(), &[0, 1], Some(KEEP), 64).unwrap();
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
        let drain = Drain::open(&run, &labels(), &[1], Some(KEEP), 64).unwrap();
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
        let drain = Drain::open(&run, &labels(), &[1], None, 64).unwrap();
        assert!(
            !drain
                .keep(&encoded(1, Channel::Stdout, 0, Event::Data(b"a\n")))
                .unwrap()
        );
        assert!(!member_dir(&run, 1).exists());
    }

    /// A run started again goes on in its members' newest segment.
    #[test]
    fn a_run_started_again_goes_on_in_its_members_logs() {
        let run = shards_testdir::TempDir::new("drain-again").unwrap();
        for word in [&b"first\n"[..], b"second\n"] {
            let drain = Drain::open(&run, &labels(), &[1], Some(KEEP), 64).unwrap();
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
