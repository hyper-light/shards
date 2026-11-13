//! A run's log host (D238's log lane): each log lane its agent-control endpoint hands its exec
//! host ([`exec::Kind::Log`]) read from the logs the run keeps, its command's (D161) and its
//! members' (D240), as the shipper and `logs -f` read them (D163), byte for byte, into the
//! lane's stream of step 5's records.
//!
//! - **Refused, typed** ([`exec::Unlogged`]): a run kept with `--log-driver none` (`Unkept`); the
//!   command's log of a run whose driver keeps no copy (`Uncached`), as `logs` refuses both; a
//!   member that runs no program of the run's (`Unknown`); a place older than the oldest segment
//!   kept (`Rotated`, naming it). The run's budget has room for what the lane holds, or it is
//!   refused `Capacity`, as an exec is.
//! - **Read** from each channel's place, the oldest kept where none is given; each record a
//!   [`exec::LogRecord`] at its own place, output in pieces of the stream's record less its head.
//!   Segments let go before it read them are said (`Rotated`), and it reads on from the oldest.
//! - **Followed** as its log is written, woken by an append to the segment it reads, a segment's
//!   coming, its source's end (the run's, for its command; the member's channels' all ended, for
//!   a member) or its lane let go; where no watch can be had, every [`UNWATCHED`] as `logs -f`
//!   reads. Unfollowed, it reads what is kept as it is asked.
//! - **Its end**: its stream finished once read, and its exit told once that is consumed. A lane
//!   let go (its controller's hangup, or its connection gone) ends its reading at once.

use std::io;
use std::path::Path;
use std::time::Duration;

use shards_abi::run::managed::Channel;
use shards_remote::control::exec::{self, LogPos, LogRecord, Reply, Start, Unlogged, Unstarted};
use shards_remote::managed::Producer;
use shards_vmm::platform::FileWatch;

use super::execs::Held;
use super::{Charge, drain, log};
use crate::daemon::UNWATCHED;
use crate::daemon::logs::{CHUNK, Kept, LogFile, Pos, Records};
use crate::spec::LOG_STDERR;

/// What the log host knows of its run: where its logs are, its members, and how each source
/// ends.
pub(super) struct Logs<'a> {
    /// The run's directory: its command's log, and `members/<index>` beside it.
    pub(super) dir: &'a Path,
    /// The members that run a program, by declared index: those whose output is kept.
    pub(super) active: &'a [u32],
    /// The keeper of its members' logs: each kept member's end.
    pub(super) drain: Option<&'a drain::Drain>,
    /// The run's end: a stream readable once it has ended, and its waiter's number to forget;
    /// none where it has ended already (`Daemon::wake_at_end`).
    pub(super) run_end: &'a (dyn Fn() -> io::Result<Option<(u64, shards_ipc::Stream)>> + Sync),
    pub(super) forget: &'a (dyn Fn(u64) + Sync),
}

/// How a followed log's source ends.
enum End {
    /// The run's: its waiter's number and the stream readable once it has ended.
    Run(u64, shards_ipc::Stream),
    /// A member's: its drain's end.
    Member(shards_ipc::Latch),
    /// Ended already, or the lane is not followed.
    Now,
}

/// What a wait woke on.
struct Woke {
    changed: bool,
    ended: bool,
    gone: bool,
}

/// Serves log lane `start` from the run's logs, charged to `charge`.
pub(super) fn serve(start: Start, logs: &Logs<'_>, admissions: exec::Admissions, charge: &Charge) {
    let Start {
        ask,
        exec: number,
        stdout,
        steer,
        replies,
        ..
    } = start;
    let exec::Kind::Log(asked) = ask.kind else {
        replies.tell(Reply::Unstarted(Unstarted::Failed));
        return;
    };
    let say = |what: &str| log(format!("agent-control log lane {number}: {what}"));
    let refuse = |why: Reply, what: &str| {
        say(what);
        replies.tell(why);
    };
    let (producer, control) = stdout;
    let opened = match source(logs, asked.source) {
        Ok(dir) => opened(logs.dir, &dir, asked),
        Err(why) => Err(why),
    };
    let (file, from) = match opened {
        Ok(opened) => opened,
        Err((why, what)) => return refuse(why, &what),
    };
    let record = producer.storage_capacity();
    let Some(piece) = record
        .checked_sub(exec::LOG_OUTPUT_HEAD)
        .filter(|_| record >= exec::LOG_RECORD_LEAST)
    else {
        return refuse(
            Reply::Unstarted(Unstarted::Failed),
            "its stream's record holds no log record",
        );
    };
    let charged = exec::lane_bytes(admissions, false, false)
        .and_then(|lanes| {
            // What it holds while it reads: its stream's record, its reader's two buffers.
            let held = u64::try_from(record.checked_add(CHUNK.checked_mul(2)?)?).ok()?;
            lanes.checked_add(held)
        })
        .ok_or("its charge overflows");
    let bytes = match charged {
        Ok(bytes) => bytes,
        Err(what) => return refuse(Reply::Unstarted(Unstarted::Failed), what),
    };
    let _held = match Held::new(charge, bytes) {
        Ok(held) => held,
        Err(e) => {
            return refuse(
                Reply::Unstarted(Unstarted::Capacity),
                &format!("{bytes} bytes: {e:?}"),
            );
        }
    };
    let end = match (asked.follow, asked.source) {
        (false, _) => Ok(End::Now),
        (true, exec::LogSource::Command) => (logs.run_end)().map(|end| match end {
            Some((waiter, stream)) => End::Run(waiter, stream),
            None => End::Now,
        }),
        (true, exec::LogSource::Member(member)) => match logs.drain.and_then(|d| d.end_of(member)) {
            Some(latch) => latch.map(End::Member),
            None => Ok(End::Now),
        },
    };
    let end = match end {
        Ok(end) => end,
        Err(e) => {
            return refuse(
                Reply::Unstarted(Unstarted::Failed),
                &format!("its source's end: {e}"),
            );
        }
    };
    let gone = match shards_ipc::Latch::new().and_then(|gone| Ok((gone.try_clone()?, gone))) {
        Ok(gone) => gone,
        Err(e) => return refuse(Reply::Unstarted(Unstarted::Failed), &format!("its wake: {e}")),
    };
    replies.tell(Reply::Started);
    let reads = [asked.channels.stdout, asked.channels.stderr];
    let mut records = Records::new(from, reads, piece);
    std::thread::scope(|scope| {
        let (let_go, waiting) = gone;
        // It takes no steering; its steering closes as the endpoint lets the lane go.
        let watched = std::thread::Builder::new()
            .name("log-lane-gone".into())
            .spawn_scoped(scope, move || {
                while steer.recv().is_ok() {}
                let_go.set();
            });
        let read = match watched {
            Ok(_) => follow(&file, &mut records, &producer, &end, &waiting),
            Err(e) => Err(io::Error::new(e.kind(), format!("its lane's watch: {e}"))),
        };
        match read {
            // Read whole: its stream finishes once its last record is consumed (a stream
            // aborted meanwhile is let go as it is).
            Ok(true) => {
                let _ = control.finish();
            }
            // Let go under it: nothing more goes.
            Ok(false) => {}
            Err(e) => {
                say(&format!("reading its log: {e}"));
                control.abort();
            }
        }
        // Told once its stream has finished, or been let go, so that its lane goes.
        replies.tell(Reply::Exited(0));
        if let End::Run(waiter, _) = &end {
            (logs.forget)(*waiter);
        }
    });
}

/// The directory of `source`'s log: the run's for its command, `members/<index>` for a member
/// that runs a program, whose log the daemon keeps whoever reads it (D240); why not, typed,
/// otherwise.
fn source(logs: &Logs<'_>, source: exec::LogSource) -> Result<std::path::PathBuf, (Reply, String)> {
    let exec::LogSource::Member(member) = source else {
        return Ok(logs.dir.to_path_buf());
    };
    if !logs.active.contains(&member) {
        return Err((
            Reply::Unlogged(Unlogged::Unknown),
            format!("member {member}, which the run has not"),
        ));
    }
    Ok(drain::member_dir(logs.dir, member))
}

/// The log in `dir` of the run whose directory is `run`, opened, and each channel's place to
/// read from: as `logs` refuses a run's log, refused; a place older than its oldest segment
/// refused `Rotated`, naming the oldest place kept; none given, the oldest place kept, or, where
/// the run's one log cap let its owner's older output go (D240), its log's first place, which its
/// reader says went (`Rotated`) before it reads from the oldest kept.
fn opened(run: &Path, dir: &Path, asked: exec::LogAsk) -> Result<(LogFile, [Pos; 2]), (Reply, String)> {
    let request = std::fs::read(run.join(crate::daemon::REQUEST))
        .ok()
        .and_then(|b| shards_ipc::Run::decode(&b));
    if let Some(r) = &request {
        if crate::logging::driver(r) == "none" {
            return Err((Reply::Unlogged(Unlogged::Unkept), "its run keeps no log".into()));
        }
        let uncached = crate::logging::kept(r, crate::daemon::DEFAULT_LOGS)
            .ok()
            .flatten()
            .is_some_and(|k| !k.cached);
        if uncached && asked.source == exec::LogSource::Command {
            return Err((
                Reply::Unlogged(Unlogged::Uncached),
                "its run's driver keeps no copy of its log".into(),
            ));
        }
    }
    let failed = |e: io::Error| (Reply::Unstarted(Unstarted::Failed), format!("its log: {e}"));
    let file = LogFile::open(dir).map_err(failed)?;
    let oldest = file
        .oldest()
        .map_err(failed)?
        .ok_or_else(|| failed(io::ErrorKind::NotFound.into()))?;
    let first = Pos::segment_start(oldest);
    let capped = super::cap::capped(file.dir()).map_err(failed)?.segments > 0;
    let start = if capped { Pos::START } else { first };
    let place = |channel| {
        let read = asked.channels.reads(channel);
        match asked.cursor.of(channel).map(|at| Pos::from_bytes(&at.to_bytes())) {
            Some(at) if read && at.seq() < oldest => Err((
                Reply::Unlogged(Unlogged::Rotated {
                    oldest: LogPos::from_bytes(&first.to_bytes()),
                }),
                format!(
                    "a place in segment {}, past which its log keeps segment {oldest} on",
                    at.seq()
                ),
            )),
            Some(at) => Ok(at),
            None => Ok(start),
        }
    };
    let from = [place(Channel::Stdout)?, place(Channel::Stderr)?];
    Ok((file, from))
}

/// Reads `file` with `records` into `producer`'s records, following it while `end` has not
/// come: whether it read all it was to (its source's end, or what was kept), or was let go
/// first.
fn follow(
    file: &LogFile,
    records: &mut Records,
    producer: &Producer,
    end: &End,
    gone: &shards_ipc::Latch,
) -> io::Result<bool> {
    let mut each = |kept: Kept<'_>| submit(producer, &kept);
    if matches!(end, End::Now) {
        return records.read(file, &mut each);
    }
    // Each watch comes before a read, so that no append between them goes unseen; where none
    // can be had (on Linux, its user's inotify instances spent: PM M257), it reads again every
    // UNWATCHED, as `logs -f` does.
    let mut watch = FileWatch::new(file.dir()).ok();
    let mut watched = None;
    loop {
        if !records.read(file, &mut each)? {
            return Ok(false);
        }
        if let Some(segment) = records.segment()
            && watched != Some(segment.seq())
            && let Some(watch) = watch.as_mut()
        {
            watch.file(segment.index())?;
            watched = Some(segment.seq());
            continue;
        }
        let woke = wait(watch.as_ref(), end, gone, watch.is_none().then_some(UNWATCHED))?;
        if woke.gone {
            return Ok(false);
        }
        if woke.ended {
            // Its source has ended, all of it kept: what is left, then its end.
            return records.read(file, &mut each);
        }
        if woke.changed
            && let Some(watch) = &watch
        {
            watch.clear();
        }
    }
}

/// Submits `kept` as one of the lane's records once its stream has room: whether it went, its
/// lane let go otherwise.
fn submit(producer: &Producer, kept: &Kept<'_>) -> io::Result<bool> {
    if producer.wait_capacity().is_err() {
        return Ok(false);
    }
    let Ok(Some(mut record)) = producer.take_buffer() else {
        return Ok(false);
    };
    let place = |pos: &Pos| LogPos::from_bytes(&pos.to_bytes());
    let channel = |stream: u8| {
        if stream == LOG_STDERR {
            Channel::Stderr
        } else {
            Channel::Stdout
        }
    };
    let written = match *kept {
        Kept::Output {
            stream,
            at,
            ref pos,
            bytes,
        } => {
            // Within the record's room: its piece is the record less this head.
            record.extend_from_slice(&exec::log_output_head(channel(stream), place(pos), at));
            record.extend_from_slice(bytes);
            Ok(())
        }
        Kept::Gap {
            stream,
            at,
            ref pos,
            lost,
            why,
        } => {
            let why = exec::Gapped::try_from(why)
                .map_err(|()| io::Error::new(io::ErrorKind::InvalidData, "a gap of no cause"))?;
            LogRecord::Gap {
                channel: channel(stream),
                pos: place(pos),
                at,
                lost,
                why,
            }
            .write_to(&mut record)
        }
        Kept::Rotated { ref oldest } => LogRecord::Rotated {
            oldest: place(oldest),
        }
        .write_to(&mut record),
    };
    written.map_err(io::Error::other)?;
    Ok(producer.submit(record).is_ok())
}

/// Waits, `within` at most, until a followed log has more to do: its watch's change, its
/// source's end, or its lane let go.
#[cfg(unix)]
fn wait(
    watch: Option<&FileWatch>,
    end: &End,
    gone: &shards_ipc::Latch,
    within: Option<Duration>,
) -> io::Result<Woke> {
    use std::os::fd::AsFd as _;
    let end_fd = match end {
        End::Run(_, stream) => Some(stream.as_fd()),
        End::Member(latch) => Some(latch.fd()),
        End::Now => None,
    };
    let fds: Vec<_> = [watch.map(FileWatch::fd), end_fd, Some(gone.fd())]
        .into_iter()
        .flatten()
        .collect();
    let ready = crate::daemon::timed(within, || shards_ipc::await_any_within(&fds, within))?;
    let mut at = ready.into_iter();
    let changed = watch.is_some() && at.next().unwrap_or(false);
    let ended = end_fd.is_some() && at.next().unwrap_or(false);
    let gone = at.next().unwrap_or(false);
    Ok(Woke { changed, ended, gone })
}

/// Waits as Unix's does: a watch's event and the latches' by their events, the run's end by
/// its stream (`shards_ipc::await_any`).
#[cfg(windows)]
fn wait(
    watch: Option<&FileWatch>,
    end: &End,
    gone: &shards_ipc::Latch,
    within: Option<Duration>,
) -> io::Result<Woke> {
    use std::os::windows::io::AsHandle as _;
    let deadline = within.and_then(|d| std::time::Instant::now().checked_add(d));
    let streams: Vec<&shards_ipc::Stream> = match end {
        End::Run(_, stream) => vec![stream],
        End::Member(_) | End::Now => Vec::new(),
    };
    let member = match end {
        End::Member(latch) => Some(latch.event().as_handle()),
        End::Run(..) | End::Now => None,
    };
    let events: Vec<_> = [watch.map(FileWatch::fd), member, Some(gone.event().as_handle())]
        .into_iter()
        .flatten()
        .collect();
    let (streams_ready, events_ready) =
        crate::daemon::timed(within, || shards_ipc::await_any(&streams, &events, deadline))?;
    let mut at = events_ready.into_iter();
    let changed = watch.is_some() && at.next().unwrap_or(false);
    let member_ended = member.is_some() && at.next().unwrap_or(false);
    let gone = at.next().unwrap_or(false);
    let ended = member_ended || streams_ready.first().copied().unwrap_or(false);
    Ok(Woke { changed, ended, gone })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::spec::{LOG_STDOUT, LogRetention};

    /// A lane asked from the oldest of a log whose older output the run's one cap let go says so
    /// first, `Rotated` with the oldest place kept, then reads from there; one whose log lost
    /// nothing to the cap reads from its oldest as before.
    #[test]
    fn a_lane_from_the_oldest_says_what_the_cap_let_go() {
        let run = shards_testdir::TempDir::new("lane-capped").unwrap();
        let newest = crate::daemon::newest_log(&run).unwrap();
        let held = shards_vmm::platform::open_dir(&run).unwrap();
        let mut writer = crate::log_writer::Writer::new(
            newest.log,
            newest.index,
            newest.seq,
            LogRetention { size: 30, files: 8 },
            Box::new(move |seq| crate::segments::new_segment(&held, seq, 8)),
        )
        .unwrap();
        for word in [&b"one\n"[..], b"two\n", b"six\n"] {
            writer.keep(LOG_STDOUT, word);
        }
        let ask = exec::LogAsk {
            source: exec::LogSource::Command,
            channels: exec::LogChannels::BOTH,
            cursor: exec::LogCursor::OLDEST,
            follow: false,
        };
        let read = |from: [Pos; 2]| {
            let mut got = Vec::new();
            Records::new(from, [true, true], 64)
                .read(&LogFile::open(&run).unwrap(), &mut |kept| {
                    got.push(match kept {
                        Kept::Output { bytes, .. } => bytes.to_vec(),
                        Kept::Rotated { oldest } => format!("rotated {}", oldest.seq()).into_bytes(),
                        Kept::Gap { .. } => b"gap".to_vec(),
                    });
                    Ok(true)
                })
                .unwrap();
            got
        };
        // Its first segment let go by its own retention: read from the oldest, nothing said.
        std::fs::remove_file(run.join("log.idx")).unwrap();
        std::fs::remove_file(run.join("log")).unwrap();
        let (_, from) = opened(&run, &run, ask).map_err(|(_, why)| why).unwrap();
        assert_eq!(read(from), [b"two\n".to_vec(), b"six\n".to_vec()]);
        // Its second let go by the run's cap, its loss recorded: said first.
        let segment = super::super::cap::tests_drop(&run, 1);
        assert_eq!(segment, 17);
        let (_, from) = opened(&run, &run, ask).map_err(|(_, why)| why).unwrap();
        assert_eq!(read(from), [b"rotated 2".to_vec(), b"six\n".to_vec()]);
    }
}
