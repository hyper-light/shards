//! The run's one log cap (D240; ROOT's ruling and the person's decision, 2026-11-13): its
//! command's log and every member's log together keep at most the run's log bound, its
//! `--log-opt max-size` times `max-file`, or else its home's `--log-history` (D186, 1 GiB unless
//! its user sets it): one bound for the run, not one each.
//!
//! - **Weighed as output**: each segment by its log's bytes as written (a packed segment's as its
//!   seek table says they were, D162), as the run's own retention bounds its log, so that a run's
//!   command alone is never cut short by its cap. Indexes are not output, and are not weighed.
//! - **The oldest goes first**, whoever's it is: of each log's oldest segment that is not its
//!   newest, the one whose index last changed longest ago (its last record's time), so that every
//!   log still loses its segments oldest first and stays contiguous, as its readers take it
//!   (segments.rs: a segment's successor says it is whole).
//! - **The disk is the authority**: the logs' segments are listed and weighed as they are on disk
//!   at each hold; nothing of them is kept between holds.
//! - **Never a log's newest**, which its writer holds. Where every log is down to its newest and
//!   they still pass the bound, the member whose newest is the oldest is closed and given a fresh
//!   one by its keeper, which holds its writer, and the closed segment goes; the run's command's
//!   newest is its VM's, never closed here. So the bound holds to within that one segment.
//! - **Each loss is its owner's record** (`log.capped` beside its log): a line of the segment let
//!   go and its bytes, which `logs` says ([`capped`]); a log lane's reader finds the segment gone
//!   and says `Rotated` with the oldest place kept.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use shards_vmm::platform;

use super::drain::member_dir;
use crate::daemon::logs::segment_of;
use crate::daemon::packed::open_log;
use crate::segments::{log_segment, packed_segment};

/// Each owner's record of what the cap let go of its log, beside its segments: not a segment's
/// name (`logs::segment_of`).
const CAPPED: &str = "log.capped";

/// Whose log: the run's command's, or a member's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    Command,
    Member(u32),
}

/// A segment the cap let go: whose, which, and the output it held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Dropped {
    pub(super) owner: Owner,
    pub(super) seq: u64,
    pub(super) bytes: u64,
}

/// What a hold let go, and the output the run's logs hold after it.
#[derive(Debug)]
pub(super) struct Held {
    pub(super) dropped: Vec<Dropped>,
    pub(super) total: u64,
}

/// What the cap let go of one owner's log, its record summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::daemon) struct Capped {
    pub(in crate::daemon) segments: u64,
    pub(in crate::daemon) bytes: u64,
}

/// One segment, as the cap weighs it.
#[derive(Debug)]
struct Segment {
    seq: u64,
    bytes: u64,
    changed: SystemTime,
}

/// One log, its segments oldest first.
#[derive(Debug)]
struct Log {
    owner: Owner,
    dir: PathBuf,
    segments: Vec<Segment>,
}

/// What one pass let go, the output left, and, where the logs still pass the bound each down to
/// its newest, the member whose newest segment its keeper is to close for the next pass.
#[derive(Debug)]
struct Pass {
    dropped: Vec<Dropped>,
    total: u64,
    close: Option<u32>,
}

/// Holds the run whose directory is `run` within `bound` bytes of output across its command's log
/// and the logs of `members`: passes until one closes no member, each member it names closed in
/// between by `rotate` (its keeper's writer, starting it a fresh segment). Each pass closes a
/// member's newest only where it holds output, and the next lets it go: at most one pass more than
/// there are members' logs.
pub(super) fn hold(
    run: &Path,
    members: &[u32],
    bound: u64,
    rotate: &mut dyn FnMut(u32) -> io::Result<()>,
) -> io::Result<Held> {
    let mut held = Held {
        dropped: Vec::new(),
        total: 0,
    };
    for _ in 0..=members.len() {
        let pass = pass(run, members, bound)?;
        held.dropped
            .try_reserve(pass.dropped.len())
            .map_err(io::Error::other)?;
        held.dropped.extend(pass.dropped);
        held.total = pass.total;
        match pass.close {
            Some(member) => rotate(member)?,
            None => break,
        }
    }
    Ok(held)
}

/// Lets go of the oldest segments across the run's logs until they hold `bound` bytes of output or
/// fewer, never a log's newest; names the member whose newest, holding output, is the oldest, where
/// they still pass it.
fn pass(run: &Path, members: &[u32], bound: u64) -> io::Result<Pass> {
    let mut logs = Vec::new();
    logs.try_reserve_exact(members.len().saturating_add(1))
        .map_err(io::Error::other)?;
    logs.push(read(Owner::Command, run.to_path_buf())?);
    for &member in members {
        logs.push(read(Owner::Member(member), member_dir(run, member))?);
    }
    let mut total = logs
        .iter()
        .flat_map(|log| &log.segments)
        .try_fold(0u64, |sum, segment| sum.checked_add(segment.bytes))
        .ok_or_else(|| io::Error::other("the run's logs' bytes overflow"))?;
    let mut dropped = Vec::new();
    while total > bound {
        // Each log's oldest that is not its newest: the one whose index last changed first.
        let oldest = logs
            .iter_mut()
            .filter(|log| log.segments.len() > 1)
            .filter_map(|log| Some((log.segments.first()?.changed, log)))
            .min_by_key(|(changed, _)| *changed)
            .map(|(_, log)| log);
        let Some(log) = oldest else {
            break;
        };
        if log.segments.is_empty() {
            break;
        }
        let segment = log.segments.remove(0);
        total = total.saturating_sub(segment.bytes);
        // One its own retention let go meanwhile is no loss of the cap's.
        if !remove(&log.dir, segment.seq)? {
            continue;
        }
        record(&log.dir, &segment)?;
        dropped.try_reserve(1).map_err(io::Error::other)?;
        dropped.push(Dropped {
            owner: log.owner,
            seq: segment.seq,
            bytes: segment.bytes,
        });
    }
    let close = if total > bound {
        logs.iter()
            .filter_map(|log| match (log.owner, log.segments.last()) {
                (Owner::Member(member), Some(newest)) if newest.bytes > 0 => Some((newest.changed, member)),
                _ => None,
            })
            .min()
            .map(|(_, member)| member)
    } else {
        None
    };
    Ok(Pass {
        dropped,
        total,
        close,
    })
}

/// The log in `dir`, its segments oldest first, each weighed as the output it holds: none where it
/// has none there.
fn read(owner: Owner, dir: PathBuf) -> io::Result<Log> {
    let held = match platform::open_dir(&dir) {
        Ok(held) => held,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Log {
                owner,
                dir,
                segments: Vec::new(),
            });
        }
        Err(e) => return Err(e),
    };
    let mut seqs: Vec<u64> = platform::names_in(&held)?
        .iter()
        .filter_map(|name| segment_of(name.to_str()?))
        .collect();
    seqs.sort_unstable();
    let mut segments = Vec::new();
    segments.try_reserve_exact(seqs.len()).map_err(io::Error::other)?;
    for seq in seqs {
        if let Some(segment) = weigh(&held, seq)? {
            segments.push(segment);
        }
    }
    Ok(Log { owner, dir, segments })
}

/// Segment `seq` of the log `dir` holds open, weighed: none where it went as it was listed (its
/// writer's retention, or the shipper's, D163).
fn weigh(dir: &File, seq: u64) -> io::Result<Option<Segment>> {
    let changed = match platform::open_in(dir, &log_segment(seq).1).and_then(|index| index.metadata()) {
        Ok(index) => index.modified()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let bytes = match open_log(dir, seq).and_then(|log| log.len()) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(Segment { seq, bytes, changed }))
}

/// Removes segment `seq` of the log in `dir`, its index first, as its writer's retention does
/// (segments.rs `new_segment`), whether its log is as written or packed. Whether its index was
/// there to remove: one gone already went by its own retention.
fn remove(dir: &Path, seq: u64) -> io::Result<bool> {
    let held = platform::open_dir(dir)?;
    let (log_name, index_name) = log_segment(seq);
    let removed = match platform::unlink_in(&held, &index_name) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    for name in [log_name, packed_segment(seq)] {
        match platform::unlink_in(&held, &name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// Appends `segment`'s loss to its owner's record in `dir`: its number and its bytes, a line.
fn record(dir: &Path, segment: &Segment) -> io::Result<()> {
    let mut file = crate::os::owner_only(std::fs::OpenOptions::new().create(true).append(true))
        .open(dir.join(CAPPED))?;
    writeln!(file, "{} {}", segment.seq, segment.bytes)
}

/// What the cap let go of the log in the directory `dir` holds open, its owner's record summed:
/// none where it let none go.
pub(in crate::daemon) fn capped(dir: &File) -> io::Result<Capped> {
    let mut text = String::new();
    match platform::open_in(dir, CAPPED) {
        Ok(mut file) => {
            std::io::Read::read_to_string(&mut file, &mut text)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Capped::default()),
        Err(e) => return Err(e),
    }
    text.lines().try_fold(Capped::default(), |sum, line| {
        let lost = line
            .split_once(' ')
            .and_then(|(_, bytes)| bytes.parse::<u64>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a cap's record that does not read"))?;
        Ok(Capped {
            segments: sum.segments.saturating_add(1),
            bytes: sum.bytes.saturating_add(lost),
        })
    })
}

/// Lets segment `seq` of the log in `dir` go as the cap does, recorded as its owner's loss: the
/// output it held. For other modules' tests of what readers make of the cap's losses.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn tests_drop(dir: &Path, seq: u64) -> u64 {
    let segment = weigh(&platform::open_dir(dir).unwrap(), seq).unwrap().unwrap();
    assert!(remove(dir, seq).unwrap());
    record(dir, &segment).unwrap();
    segment.bytes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::log_writer::Writer;
    use crate::spec::{LOG_STDOUT, LogRetention};

    /// A writer of the log in `dir`, segments of `size` bytes, as many as `files`.
    fn writer(dir: &Path, size: u64, files: u64) -> Writer {
        std::fs::create_dir_all(dir).unwrap();
        let newest = crate::daemon::newest_log(dir).unwrap();
        let held = platform::open_dir(dir).unwrap();
        Writer::new(
            newest.log,
            newest.index,
            newest.seq,
            LogRetention { size, files },
            Box::new(move |seq| crate::segments::new_segment(&held, seq, files)),
        )
        .unwrap()
    }

    /// The segments the log in `dir` has, oldest first.
    fn kept(dir: &Path) -> Vec<u64> {
        read(Owner::Command, dir.to_path_buf())
            .unwrap()
            .segments
            .iter()
            .map(|s| s.seq)
            .collect()
    }

    /// Ages the index of segment `seq` of the log in `dir` to `secs` past the epoch: what the cap
    /// orders its logs' oldest by.
    fn age(dir: &Path, seq: u64, secs: u64) {
        let index = std::fs::File::options()
            .write(true)
            .open(dir.join(log_segment(seq).1))
            .unwrap();
        index
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    /// What the cap let go of the log in `dir`, as `logs` reads it.
    fn capped_at(dir: &Path) -> Capped {
        capped(&platform::open_dir(dir).unwrap()).unwrap()
    }

    /// The output of the logs of the run in `run` and of `members`, as the cap weighs it.
    fn total(run: &Path, members: &[u32]) -> u64 {
        std::iter::once(run.to_path_buf())
            .chain(members.iter().map(|&m| member_dir(run, m)))
            .map(|dir| read(Owner::Command, dir).unwrap())
            .flat_map(|log| log.segments)
            .map(|s| s.bytes)
            .sum()
    }

    /// A drop across owners records the loss per owner: the oldest segment across the run's logs
    /// goes first, whoever's it is. The command's and a member's logs of three segments each, aged
    /// in turn, under a bound of four segments' output: the oldest two across both go, one each,
    /// and each owner's record says its own.
    #[test]
    fn a_drop_across_owners_records_the_loss_per_owner() {
        let run = shards_testdir::TempDir::new("cap-oldest").unwrap();
        let member = member_dir(&run, 1);
        // Records of 17 bytes in segments of 30: one a segment.
        let (mut command, mut agent) = (writer(&run, 30, 8), writer(&member, 30, 8));
        for _ in 0..3 {
            command.keep(LOG_STDOUT, b"cmd\n");
            agent.keep(LOG_STDOUT, b"mbr\n");
        }
        // The command's first is the oldest, then the member's first, then the command's second.
        for (dir, seq, secs) in [(&*run, 0, 10), (&member, 0, 20), (&*run, 1, 30), (&member, 1, 40)] {
            age(dir, seq, secs);
        }
        let segment = total(&run, &[1]) / 6;
        assert_eq!(segment, 17, "a record of 4 bytes and its head");
        let held = hold(&run, &[1], 4 * segment, &mut |_| {
            Err(io::Error::other("none to close"))
        })
        .unwrap();
        assert_eq!(
            held.dropped,
            [
                Dropped {
                    owner: Owner::Command,
                    seq: 0,
                    bytes: segment
                },
                Dropped {
                    owner: Owner::Member(1),
                    seq: 0,
                    bytes: segment
                },
            ]
        );
        assert_eq!(held.total, 4 * segment);
        assert_eq!((kept(&run), kept(&member)), (vec![1, 2], vec![1, 2]));
        let one = Capped {
            segments: 1,
            bytes: segment,
        };
        assert_eq!((capped_at(&run), capped_at(&member)), (one, one));
    }

    /// Each log loses its segments oldest first and keeps its newest, whatever their ages: a
    /// member's newest aged oldest of all is not taken while the logs have older segments, and
    /// one log alone past the bound loses its oldest down to its newest, never that.
    #[test]
    fn each_log_loses_its_oldest_first_and_keeps_its_newest() {
        let run = shards_testdir::TempDir::new("cap-own").unwrap();
        let member = member_dir(&run, 0);
        let (mut command, mut agent) = (writer(&run, 30, 8), writer(&member, 30, 8));
        for _ in 0..4 {
            command.keep(LOG_STDOUT, b"cmd\n");
        }
        agent.keep(LOG_STDOUT, b"mbr\n");
        // The member's one segment, its newest, last changed before any of the command's.
        age(&member, 0, 1);
        for seq in 0..4 {
            age(&run, seq, 100 + seq);
        }
        // Nothing to close: the member's newest is closed only once the command is down to its
        // newest, and then the command's own newest stays.
        let mut closed = Vec::new();
        let held = hold(&run, &[0], 0, &mut |member| {
            closed.push(member);
            Ok(())
        })
        .unwrap();
        let commands: Vec<u64> = held
            .dropped
            .iter()
            .filter(|d| d.owner == Owner::Command)
            .map(|d| d.seq)
            .collect();
        assert_eq!(commands, [0, 1, 2], "{:?}", held.dropped);
        assert_eq!(kept(&run), [3], "the command's newest is its VM's");
        assert!(closed.iter().all(|&m| m == 0), "{closed:?}");
    }

    /// The newest-segment rotation keeps the bound to within one segment: more logs than the
    /// bound holds segments, each down to its newest, the oldest member's newest is closed by its
    /// keeper and goes, in turn, until the logs hold the bound or less, the command's newest, which
    /// is its VM's, kept; each loss recorded.
    #[test]
    fn the_newest_segment_rotation_keeps_the_bound_within_one_segment() {
        let run = shards_testdir::TempDir::new("cap-many").unwrap();
        let members = [0u32, 1, 2, 3];
        let mut command = writer(&run, 30, 8);
        command.keep(LOG_STDOUT, b"cmd\n");
        let mut agents: Vec<Writer> = members
            .iter()
            .map(|&m| writer(&member_dir(&run, m), 30, 8))
            .collect();
        for (at, agent) in agents.iter_mut().enumerate() {
            agent.keep(LOG_STDOUT, b"mbr\n");
            age(&member_dir(&run, at as u32), 0, 10 + at as u64);
        }
        age(&run, 0, 1);
        let segment = total(&run, &members) / 5;
        // Two segments' output for five logs of one segment each.
        let bound = 2 * segment;
        let mut closed = Vec::new();
        let held = hold(&run, &members, bound, &mut |member| {
            closed.push(member);
            agents[member as usize].rotate()
        })
        .unwrap();
        assert_eq!(closed, [0, 1, 2], "the oldest members' newest, in turn");
        assert_eq!(held.dropped.len(), 3, "{:?}", held.dropped);
        assert_eq!(held.total, total(&run, &members));
        assert!(held.total <= bound, "{} past {bound}", held.total);
        assert_eq!(kept(&run), [0], "the command's newest is its VM's");
        for member in [0, 1, 2] {
            assert_eq!(
                capped_at(&member_dir(&run, member)),
                Capped {
                    segments: 1,
                    bytes: segment
                }
            );
        }
        assert_eq!(capped_at(&member_dir(&run, 3)), Capped::default());
    }
}
