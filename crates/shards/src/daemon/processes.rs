//! A run's record of the processes its members start and end (AGENTFILE_ARCH.md §9.9,
//! D123): what its guest's init sends (`kind::PROCESS`), kept in its container's directory
//! as its log is kept, in segments `processes.0`, `processes.1` and on, each of the log's
//! size at most and as many as its files, the oldest removed first; `shards read
//! processes` shows it, `shards watch processes` follows it as it grows.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use super::commands::{Reply, wait_readable};
use super::{Daemon, lock};
use crate::containers::Disk;

/// A segment's name, before its number.
const SEGMENT: &str = "processes.";

/// Linux's signals by number, as a guest's are (asm-generic/signal.h, the same on x86_64
/// and arm64 for these).
const SIGNALS: [&str; 32] = [
    "",
    "SIGHUP",
    "SIGINT",
    "SIGQUIT",
    "SIGILL",
    "SIGTRAP",
    "SIGABRT",
    "SIGBUS",
    "SIGFPE",
    "SIGKILL",
    "SIGUSR1",
    "SIGSEGV",
    "SIGUSR2",
    "SIGPIPE",
    "SIGALRM",
    "SIGTERM",
    "SIGSTKFLT",
    "SIGCHLD",
    "SIGCONT",
    "SIGSTOP",
    "SIGTSTP",
    "SIGTTIN",
    "SIGTTOU",
    "SIGURG",
    "SIGXCPU",
    "SIGXFSZ",
    "SIGVTALRM",
    "SIGPROF",
    "SIGWINCH",
    "SIGIO",
    "SIGPWR",
    "SIGSYS",
];

/// The record's segments in `dir`, oldest first, each with its number.
fn segments(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut out: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name();
                    let n = name.to_str()?.strip_prefix(SEGMENT)?.parse().ok()?;
                    Some((n, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|&(n, _)| n);
    out
}

/// Keeps `records`, whole lines, in the record in container directory `dir`: in its last
/// segment, or a new one where that would pass `size`, the oldest past `files` removed.
fn keep(dir: &Path, records: &[u8], size: u64, files: u64) -> io::Result<()> {
    let all = segments(dir);
    let (mut n, at) = match all.last() {
        Some((n, path)) => (*n, std::fs::metadata(path).map_or(0, |m| m.len())),
        None => (0, 0),
    };
    let len = u64::try_from(records.len()).unwrap_or(u64::MAX);
    if at > 0 && at.saturating_add(len) > size {
        n = n.saturating_add(1);
        for (old, path) in &all {
            if n.saturating_sub(*old) >= files.max(1) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{SEGMENT}{n}")))?
        .write_all(records)
}

/// The record's whole lines in `dir`, from `from` on: each segment from its number's byte,
/// and where reading stopped, for the next.
fn read(dir: &Path, from: (u64, u64)) -> (Vec<String>, (u64, u64)) {
    let mut lines = Vec::new();
    let mut at = from;
    for (n, path) in segments(dir) {
        if n < from.0 {
            continue;
        }
        let skip = if n == from.0 { from.1 } else { 0 };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let rest = bytes
            .get(usize::try_from(skip).unwrap_or(usize::MAX)..)
            .unwrap_or_default();
        // Whole lines alone: a line being written is read once it is.
        let whole = rest.iter().rposition(|&b| b == b'\n').map_or(0, |end| end + 1);
        for line in rest.get(..whole).unwrap_or_default().split(|&b| b == b'\n') {
            if !line.is_empty() {
                lines.push(String::from_utf8_lossy(line).into_owned());
            }
        }
        at = (n, skip.saturating_add(u64::try_from(whole).unwrap_or(0)));
    }
    (lines, at)
}

/// A record's fields as shown: its time, member, PID, event and what it says.
fn shown(line: &str) -> Option<[String; 5]> {
    let mut f = line.splitn(5, '\t');
    let (time, member, pid, event, says) =
        (f.next()?, f.next()?, f.next()?, f.next()?, f.next().unwrap_or(""));
    let time = time.parse::<u64>().ok().map(super::commands::rfc3339_nano)?;
    let says = match (event, says.split_once(' ')) {
        ("exit", Some(("signal", n))) => {
            let name = n
                .parse::<usize>()
                .ok()
                .and_then(|n| SIGNALS.get(n))
                .copied()
                .unwrap_or("");
            if name.is_empty() {
                says.to_string()
            } else {
                format!("signal {n} ({name})")
            }
        }
        ("fork", _) => format!("from {says}"),
        ("exec", _) => command_line(says),
        ("lost", _) => "events the guest's queue could not hold".to_string(),
        _ => says.to_string(),
    };
    Some([time, member.to_string(), pid.to_string(), event.to_string(), says])
}

/// An exec's command line as shown, from what its record says: the arguments, then where
/// they are not all of it, what is not: the bytes left out, or the process's name alone,
/// in brackets as ps shows one without a command line, or that none could be read.
fn command_line(says: &str) -> String {
    match says.split_once('\t') {
        Some((name, "comm")) => format!("[{name}]"),
        Some((_, "gone")) => "(ended before its command line could be read)".to_string(),
        Some((line, more)) => match more.strip_prefix('+') {
            Some(n) => format!("{line} (and {n} bytes more)"),
            None => line.to_string(),
        },
        None => says.to_string(),
    }
}

/// Each field's width: at least its heading's, and each line's.
fn widths(rows: &[[String; 5]]) -> [usize; 4] {
    let mut w = ["TIME".len(), "MEMBER".len(), "PID".len(), "EVENT".len()];
    for r in rows {
        for (i, f) in r.iter().take(4).enumerate() {
            if let Some(w) = w.get_mut(i) {
                *w = (*w).max(f.len());
            }
        }
    }
    w
}

/// A row laid out at `w`.
fn row([time, member, pid, event, says]: &[String; 5], [a, b, c, d]: [usize; 4]) -> String {
    format!("{time:<a$}  {member:<b$}  {pid:>c$}  {event:<d$}  {says}")
}

impl<D: Disk> Daemon<D> {
    /// Keeps `records` in container `id`'s record of processes, as its log is kept.
    pub(super) fn keep_processes(&self, id: &str, records: &[u8]) {
        let dir = lock(&self.containers).dir(id);
        let kept = self.log_retention().unwrap_or_else(|e| {
            super::log(format!(
                "container {id}: its record of processes, kept as by default: {e}"
            ));
            super::DEFAULT_LOGS
        });
        if let Err(e) = keep(&dir, records, kept.size, kept.files) {
            super::log(format!("container {id}: its record of processes: {e}"));
        }
    }

    /// `shards read processes MICROVM` (D123): the processes its run's members started and
    /// ended, as its record holds them; with `follow` (`shards watch processes`), and as
    /// they come, until its run ends or its client goes.
    pub(super) fn read_processes(&self, args: &[String], follow: bool, client: u64, reply: &Reply<'_>) -> u8 {
        let Some(reference) = args.first() else {
            return 1;
        };
        let id = match self.resolve(reference) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        let dir = lock(&self.containers).dir(&id);
        let (lines, mut at) = read(&dir, (0, 0));
        let rows: Vec<[String; 5]> = lines.iter().filter_map(|l| shown(l)).collect();
        let w = widths(&rows);
        let heading = ["TIME", "MEMBER", "PID", "EVENT", "WHAT"].map(String::from);
        reply.out(&row(&heading, w));
        for r in &rows {
            reply.out(&row(r, w));
        }
        if !follow || !self.running(&id) {
            return 0;
        }
        // Followed as it grows: woken by an append to its last segment or a segment's
        // coming, by the run's end, or by the client's hanging up, as `logs -f` is; where
        // no watch can be had, it reads again every RETRY, as `logs -f` does.
        let followed = (|| -> io::Result<()> {
            let mut watch = shards_vmm::platform::FileWatch::new(&std::fs::File::open(&dir)?).ok();
            // The segment watched, by its number.
            let mut watched: Option<u64> = None;
            let Some((number, end)) = self.wake_at_end(&id)? else {
                return Ok(());
            };
            self.waits_long(client);
            let result = (|| -> io::Result<()> {
                loop {
                    if let Some((n, path)) = segments(&dir).last()
                        && watched != Some(*n)
                        && let Some(watch) = watch.as_mut()
                    {
                        watch.file(&std::fs::File::open(path)?)?;
                        watched = Some(*n);
                    }
                    let (lines, next) = read(&dir, at);
                    at = next;
                    for l in &lines {
                        if let Some(r) = shown(l) {
                            reply.out(&row(&r, w));
                        }
                    }
                    let ready = wait_readable(
                        watch.as_ref(),
                        &end,
                        reply.0,
                        watch.is_none().then_some(super::RETRY),
                    )?;
                    if ready[1] {
                        for l in &read(&dir, at).0 {
                            if let Some(r) = shown(l) {
                                reply.out(&row(&r, w));
                            }
                        }
                        return Ok(());
                    }
                    if ready[2] {
                        return Ok(());
                    }
                    if let Some(watch) = &watch {
                        watch.clear();
                    }
                }
            })();
            self.forget_waiter(&id, number);
            result
        })();
        if let Err(e) = followed {
            reply.err(&format!("shards: {reference}: its record of processes: {e}"));
            return 1;
        }
        0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The record kept as the log is: a new segment where the last would pass the size,
    /// the oldest past the count removed, whole lines read in order across them, and a
    /// line being written read once it is whole.
    #[test]
    fn the_record_is_kept_in_segments_as_the_log_is() {
        let dir = shards_testdir::TempDir::new("processes").unwrap();
        for n in 0..5 {
            keep(&dir, format!("{n}\tcommand\t{n}\texec\tx\n").as_bytes(), 40, 2).unwrap();
        }
        // 19 bytes each, two to a segment: 0 and 1, 2 and 3, then 4; the last two kept.
        let names: Vec<u64> = segments(&dir).iter().map(|(n, _)| *n).collect();
        assert_eq!(names, [1, 2], "two segments kept");
        let (lines, at) = read(&dir, (0, 0));
        assert_eq!(
            lines,
            [
                "2\tcommand\t2\texec\tx",
                "3\tcommand\t3\texec\tx",
                "4\tcommand\t4\texec\tx"
            ]
        );
        keep(&dir, b"5\tcommand\t5\texit\tcode 0\n6\tcomm", 1 << 20, 2).unwrap();
        let (lines, at) = read(&dir, at);
        assert_eq!(lines, ["5\tcommand\t5\texit\tcode 0"], "the part of a line waits");
        keep(&dir, b"and\t6\tfork\t5\n", 1 << 20, 2).unwrap();
        assert_eq!(read(&dir, at).0, ["6\tcommand\t6\tfork\t5"]);
    }

    /// Records as shown: the time as `logs -t` writes one, a signal named, a parent said,
    /// an exec's command line with what it left out.
    #[test]
    fn records_are_shown_with_their_names() {
        let [time, member, pid, event, says] =
            shown("1760149867123456789\tagent web\t12\texit\tsignal 9").unwrap();
        assert_eq!(
            (
                time.as_str(),
                member.as_str(),
                pid.as_str(),
                event.as_str(),
                says.as_str()
            ),
            (
                "2025-10-11T02:31:07.123456789Z",
                "agent web",
                "12",
                "exit",
                "signal 9 (SIGKILL)"
            )
        );
        assert_eq!(shown("1\tcommand\t7\tfork\t1").unwrap()[4], "from 1");
        assert_eq!(shown("1\tcommand\t7\texit\tcode 3").unwrap()[4], "code 3");
        assert_eq!(shown("1\tcommand\t7\texec\tsh -c \"\"").unwrap()[4], "sh -c \"\"");
        assert_eq!(
            shown("1\tcommand\t7\texec\techo aaa\t+4096").unwrap()[4],
            "echo aaa (and 4096 bytes more)"
        );
        assert_eq!(shown("1\tcommand\t7\texec\ttrue\tcomm").unwrap()[4], "[true]");
        assert_eq!(
            shown("1\tcommand\t7\texec\t\tgone").unwrap()[4],
            "(ended before its command line could be read)"
        );
        assert!(shown("not a record").is_none());
    }
}
