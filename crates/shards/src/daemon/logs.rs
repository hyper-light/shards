//! Reading a container's log (spec.rs, `LOG_STDOUT`) for `shards logs` (audit A12):
//! through its index, in buffers of bounded size however long it is or its lines are, and
//! back from its end for `--tail`, reading only what the lines it shows hold.
//!
//! A line is held until it ends or reaches [`PIECE`] bytes, then goes out in pieces, its
//! prefix before the first: the bytes a client gets are the whole line's, as moby's copier
//! splits a long line into partial messages (daemon/logger/copier.go, `bufSize`).
//!
//! Lines are ordered by where their last byte is. A line a container left unfinished
//! counts, once it has ended, where its last byte is.

use std::fs::File;
use std::io::{self, Write as _};
use std::os::unix::fs::FileExt;
use std::path::Path;

use shards_vmm::platform;

use crate::spec::{INDEX_LINE, INDEX_START, INDEX_STDERR, LOG_HEAD, LOG_STDERR, LOG_STDOUT, log_segment};

/// A log's first segment, and its index, in its container's directory.
pub const LOG: &str = "log";
pub const INDEX: &str = "log.idx";
/// How much is read at once.
const CHUNK: usize = 64 << 10;
/// How much of a line is held before it goes out in pieces: moby's copier's buffer.
pub const PIECE: usize = 16 << 10;
/// Past every record there will be.
const NEVER: Pos = Pos {
    seq: u64::MAX,
    no: 0,
    byte: 0,
};

/// A byte of the log: its segment, its record's number there, and where in the record's
/// output it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    seq: u64,
    no: u64,
    byte: u64,
}

impl Pos {
    const START: Pos = Pos {
        seq: 0,
        no: 0,
        byte: 0,
    };
}

/// A record: its stream (0 for stdout, 1 for stderr), when its output came, and where the
/// output is in its segment's log.
#[derive(Debug, Clone, Copy)]
struct Record {
    stream: usize,
    at: u64,
    start: u64,
    len: u64,
}

/// A container's log: its directory, where its segments are (spec.rs, `log_segment`),
/// held open wherever it goes.
#[derive(Debug)]
pub struct LogFile {
    dir: File,
}

/// A segment of a log, opened: read on, whatever its writer removes since.
#[derive(Debug)]
pub struct Segment {
    seq: u64,
    log: File,
    index: File,
}

impl LogFile {
    /// The log in `dir`. One an earlier shards wrote, without an index, is indexed first:
    /// nothing writes it any more, since a daemon of another build ends the runs of the
    /// one it replaces. A log with no segment there is none.
    pub fn open(dir: &Path) -> io::Result<LogFile> {
        let log = LogFile {
            dir: platform::open_dir(dir)?,
        };
        let mut segments = log.segments()?;
        // An earlier shards's log is its only file; this one's first log is without its
        // index only as it goes, once a later segment is there.
        if segments.is_empty()
            && let Some(first) = not_found_as_none(platform::open_in(&log.dir, LOG))?
        {
            index_legacy(dir, &first)?;
            segments = log.segments()?;
        }
        if segments.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no log there"));
        }
        Ok(log)
    }

    /// The segments there, oldest first.
    fn segments(&self) -> io::Result<Vec<u64>> {
        let mut seqs: Vec<u64> = platform::names_in(&self.dir)?
            .iter()
            .filter_map(|name| segment_of(name.to_str()?))
            .collect();
        seqs.sort_unstable();
        Ok(seqs)
    }

    /// Its directory, open.
    pub fn dir(&self) -> &File {
        &self.dir
    }

    /// Segment `seq`, or `None` if it is not there: not yet, or no longer.
    fn segment(&self, seq: u64) -> io::Result<Option<Segment>> {
        let (_, index_name) = log_segment(seq);
        let Some(index) = not_found_as_none(platform::open_in(&self.dir, &index_name))? else {
            return Ok(None);
        };
        self.segment_of_index(seq, index)
    }

    /// Segment `seq`, its index opened.
    fn segment_of_index(&self, seq: u64, index: File) -> io::Result<Option<Segment>> {
        let (log_name, index_name) = log_segment(seq);
        match platform::open_in(&self.dir, &log_name) {
            Ok(log) => Ok(Some(Segment { seq, log, index })),
            // Removed since its index was opened, as its writer removes it, index first;
            // with its index still there, it is damage.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match not_found_as_none(platform::open_in(&self.dir, &index_name))? {
                    None => Ok(None),
                    Some(_) => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{log_name} is missing"),
                    )),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The first segment there from `seq` on: `seq`'s, or, once it is gone, the next.
    fn first_from(&self, seq: u64) -> io::Result<Option<Segment>> {
        let mut want = seq;
        loop {
            if let Some(segment) = self.segment(want)? {
                return Ok(Some(segment));
            }
            match self.segments()?.into_iter().find(|&s| s > want) {
                Some(later) => want = later,
                None => return Ok(None),
            }
        }
    }

    /// Whether a segment after `seq` is there, which says `seq`'s is whole: its writer
    /// writes only its last.
    fn has_after(&self, seq: u64) -> io::Result<bool> {
        let (_, index_name) = log_segment(seq.saturating_add(1));
        if not_found_as_none(platform::open_in(&self.dir, &index_name))?.is_some() {
            return Ok(true);
        }
        Ok(self.segments()?.last().is_some_and(|&last| last > seq))
    }
}

impl Segment {
    /// Records whole so far.
    fn count(&self) -> io::Result<u64> {
        Ok(self.index.metadata()?.len() / 8)
    }

    /// Record `no`'s index entry.
    fn entry(&self, no: u64) -> io::Result<u64> {
        let mut e = [0u8; 8];
        self.index.read_exact_at(&mut e, no.saturating_mul(8))?;
        Ok(u64::from_be_bytes(e))
    }

    /// Record `no`, checked against its entry: a record that is not where its index says,
    /// or not whole, is damage, not output.
    fn record(&self, no: u64) -> io::Result<Record> {
        let entry = self.entry(no)?;
        let at = entry & INDEX_START;
        let mut head = [0u8; LOG_HEAD as usize];
        self.log.read_exact_at(&mut head, at)?;
        let [stream, t @ .., l0, l1, l2, l3] = head;
        let damaged = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the log is damaged at record {no} of its segment {}", self.seq),
            )
        };
        let expected = if entry & INDEX_STDERR != 0 {
            LOG_STDERR
        } else {
            LOG_STDOUT
        };
        if stream != expected {
            return Err(damaged());
        }
        let len = u64::from(u32::from_be_bytes([l0, l1, l2, l3]));
        let start = at + LOG_HEAD;
        if start
            .checked_add(len)
            .is_none_or(|end| end > self.log.metadata().map_or(0, |m| m.len()))
        {
            return Err(damaged());
        }
        Ok(Record {
            stream: usize::from(stream == LOG_STDERR),
            at: u64::from_be_bytes(t),
            start,
            len,
        })
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Its index, which grows as its writer appends to it.
    pub fn index(&self) -> &File {
        &self.index
    }
}

/// The segment a file `name` is the index of.
fn segment_of(name: &str) -> Option<u64> {
    if name == INDEX {
        return Some(0);
    }
    let seq: u64 = name.strip_prefix("log.")?.strip_suffix(".idx")?.parse().ok()?;
    // Only the name its writer gives it: `log.01.idx` is not a segment.
    (seq > 0 && log_segment(seq).1 == name).then_some(seq)
}

fn not_found_as_none<T>(r: io::Result<T>) -> io::Result<Option<T>> {
    match r {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Indexes a log an earlier shards wrote, as its writer would have (workload.rs,
/// `Logger`), reading it once forward; a record cut short at its end is left out. The
/// index is written aside and renamed into place, so a crash leaves none or all of it.
fn index_legacy(dir: &Path, log: &File) -> io::Result<()> {
    let len = log.metadata()?.len();
    let aside = dir.join(format!("{INDEX}.new"));
    let mut out = io::BufWriter::new(File::create(&aside)?);
    let mut at = 0u64;
    while at + LOG_HEAD <= len {
        let mut head = [0u8; LOG_HEAD as usize];
        log.read_exact_at(&mut head, at)?;
        let [stream, .., l0, l1, l2, l3] = head;
        let size = u64::from(u32::from_be_bytes([l0, l1, l2, l3]));
        let end = at + LOG_HEAD + size;
        if end > len || !matches!(stream, LOG_STDOUT | LOG_STDERR) {
            break;
        }
        // Nothing is no output: the writer keeps no such record now.
        if size == 0 {
            at = end;
            continue;
        }
        let mut entry = at;
        if stream == LOG_STDERR {
            entry |= INDEX_STDERR;
        }
        let mut last = [0u8; 1];
        log.read_exact_at(&mut last, end - 1)?;
        if last[0] == b'\n' {
            entry |= INDEX_LINE;
        }
        out.write_all(&entry.to_be_bytes())?;
        at = end;
    }
    out.into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .sync_all()?;
    std::fs::rename(&aside, dir.join(INDEX))
}

/// Where `--tail n` starts, for each stream: the first byte of the first of the last `n`
/// lines of it, or the log's end, which lines to come are past. With `ended`, the lines a
/// container left unfinished count; without, they are still to come, and are read from
/// where they began, whatever `n`. Read back from the end, segment by segment: the index,
/// read back in chunks, says each record's stream and whether it ends a line, and only
/// the records of lines whose starts are sought are read.
pub fn tail(log: &LogFile, n: u64, ended: bool) -> io::Result<[Pos; 2]> {
    #[derive(Clone, Copy, PartialEq)]
    enum Seek {
        /// Nothing of the stream seen yet, from the end.
        Unseen,
        /// A line of it is counted, or is an unfinished one to come: its start is sought.
        Open,
        Done,
    }
    /// A stream as the walk back finds it: what is sought of it, and where its lines
    /// to show start.
    #[derive(Clone, Copy)]
    struct Stream {
        seek: Seek,
        from: Pos,
    }
    let mut streams: Option<[Stream; 2]> = None;
    let mut counted = 0u64;
    let mut buf = vec![0u8; CHUNK];
    'segments: for seq in log.segments()?.into_iter().rev() {
        // One gone as it was found: those before it are gone too.
        let Some(segment) = log.segment(seq)? else {
            break;
        };
        let count = segment.count()?;
        let streams = streams.get_or_insert(
            [Stream {
                seek: Seek::Unseen,
                from: Pos {
                    seq,
                    no: count,
                    byte: 0,
                },
            }; 2],
        );
        let mut entries = Backward::new(&segment, count);
        while let Some((no, entry)) = entries.next()? {
            let ends_line = entry & INDEX_LINE != 0;
            let Some(stream) = streams.get_mut(usize::from(entry & INDEX_STDERR != 0)) else {
                continue;
            };
            let latest = stream.seek == Seek::Unseen;
            if latest {
                // The stream's last byte: a line ends there, or one is left unfinished.
                stream.seek = if !ends_line && !ended {
                    Seek::Open
                } else if counted < n {
                    counted += 1;
                    Seek::Open
                } else {
                    Seek::Done
                };
            }
            if stream.seek == Seek::Open {
                // Back through the record's output for the newline before the open line.
                let record = segment.record(no)?;
                let mut left = record.len;
                // The stream's latest line, counted above, ends with this record's last byte.
                let mut skip_last = latest && ends_line && record.len > 0;
                'record: while left > 0 {
                    let take = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
                    let begin = left - take as u64;
                    let chunk = buf.get_mut(..take).unwrap_or_default();
                    segment.log.read_exact_at(chunk, record.start + begin)?;
                    for (i, &b) in chunk.iter().enumerate().rev() {
                        if std::mem::take(&mut skip_last) || b != b'\n' {
                            continue;
                        }
                        stream.from = Pos {
                            seq,
                            no,
                            byte: begin + i as u64 + 1,
                        };
                        if counted < n {
                            counted += 1;
                        } else {
                            stream.seek = Seek::Done;
                            break 'record;
                        }
                    }
                    left = begin;
                }
                if stream.seek == Seek::Open {
                    stream.from = Pos { seq, no, byte: 0 };
                }
            }
            // Once nothing is sought, only an unseen stream's unfinished line, still to
            // come, is worth looking further back for.
            let open = streams.iter().any(|s| s.seek == Seek::Open);
            let unseen = streams.iter().any(|s| s.seek == Seek::Unseen);
            if !open && (!unseen || ended && counted >= n) {
                break 'segments;
            }
        }
    }
    Ok(streams.map_or([Pos::START; 2], |s| s.map(|s| s.from)))
}

/// A segment's index entries from the end back, read in chunks.
struct Backward<'a> {
    segment: &'a Segment,
    next: u64,
    buf: Vec<u8>,
    /// The number of the first entry in `buf`.
    first: u64,
}

impl<'a> Backward<'a> {
    fn new(segment: &'a Segment, count: u64) -> Backward<'a> {
        Backward {
            segment,
            next: count,
            buf: vec![0u8; CHUNK],
            first: count,
        }
    }

    fn next(&mut self) -> io::Result<Option<(u64, u64)>> {
        if self.next == 0 {
            return Ok(None);
        }
        let no = self.next - 1;
        if no < self.first {
            let per = (CHUNK / 8) as u64;
            let first = no.saturating_sub(per - 1);
            let held = no - first + 1;
            let bytes = self
                .buf
                .get_mut(..usize::try_from(held * 8).unwrap_or(CHUNK))
                .unwrap_or_default();
            self.segment.index.read_exact_at(bytes, first * 8)?;
            self.first = first;
        }
        let at = usize::try_from((no - self.first) * 8).unwrap_or(0);
        let entry = self
            .buf
            .get(at..at + 8)
            .and_then(|e| e.try_into().ok())
            .map(u64::from_be_bytes)
            .ok_or_else(|| io::Error::other("an index entry out of its chunk"))?;
        self.next = no;
        Ok(Some((no, entry)))
    }
}

/// A line going out, its first piece, or a later one.
pub struct Piece<'a> {
    pub stream: u8,
    /// When the line's first byte came.
    pub at: u64,
    /// Whether this is the line's first piece, which its prefix goes before.
    pub first: bool,
    pub bytes: &'a [u8],
}

/// A line held until it ends or its piece is full.
#[derive(Debug)]
struct Open {
    at: u64,
    started: bool,
    held: Vec<u8>,
}

/// Where a reader of the log is: the next record, the segment it is read from, and each
/// stream's line in progress. Lines run on from one segment into the next; those whose
/// segments are gone before it reads them are gone from what it shows, as Docker's
/// rotated logs lose theirs.
#[derive(Debug)]
pub struct Reader {
    next: Pos,
    segment: Option<Segment>,
    from: [Pos; 2],
    open: [Option<Open>; 2],
}

/// Whether a reader goes on after a piece.
pub type Flow = io::Result<bool>;

impl Reader {
    /// A reader of the whole log.
    pub fn new() -> Reader {
        Reader::from([Pos::START; 2])
    }

    /// A reader of the lines of each stream starting at or after `from`'s place for it
    /// ([`tail`]).
    pub fn from(from: [Pos; 2]) -> Reader {
        let first = from.iter().min().copied().unwrap_or(Pos::START);
        Reader {
            next: Pos { byte: 0, ..first },
            segment: None,
            from,
            open: [None, None],
        }
    }

    /// The segment read last, to watch for more of it.
    pub fn segment(&self) -> Option<&Segment> {
        self.segment.as_ref()
    }

    /// Reads the records whole so far, and gives each line's pieces to `each` as they
    /// are ready. Whether to go on: `each` may say no.
    pub fn read(&mut self, log: &LogFile, each: &mut dyn FnMut(Piece<'_>) -> Flow) -> Flow {
        let mut buf = vec![0u8; CHUNK];
        loop {
            if self.segment.as_ref().map(|s| s.seq) != Some(self.next.seq) {
                let Some(segment) = log.first_from(self.next.seq)? else {
                    return Ok(true);
                };
                if segment.seq != self.next.seq {
                    self.next = Pos {
                        seq: segment.seq,
                        ..Pos::START
                    };
                }
                self.segment = Some(segment);
            }
            let Some(segment) = self.segment.take() else {
                return Ok(true);
            };
            // Whether it is whole is asked before its records are counted: records it
            // gets after the count are only before a later segment's.
            let read = log
                .has_after(segment.seq)
                .and_then(|whole| Ok((whole, self.read_segment(&segment, &mut buf, each)?)));
            let seq = segment.seq;
            self.segment = Some(segment);
            match read? {
                (_, false) => return Ok(false),
                (false, true) => return Ok(true),
                (true, true) => {}
            }
            self.next = Pos {
                seq: seq.saturating_add(1),
                ..Pos::START
            };
        }
    }

    /// Reads `segment`'s records whole so far, from the next.
    fn read_segment(
        &mut self,
        segment: &Segment,
        buf: &mut [u8],
        each: &mut dyn FnMut(Piece<'_>) -> Flow,
    ) -> Flow {
        let count = segment.count()?;
        while self.next.no < count {
            let no = self.next.no;
            let record = segment.record(no)?;
            let mut done = 0u64;
            while done < record.len {
                let take = usize::try_from((record.len - done).min(CHUNK as u64)).unwrap_or(CHUNK);
                let chunk = buf.get_mut(..take).unwrap_or_default();
                segment.log.read_exact_at(chunk, record.start + done)?;
                let at = Pos {
                    seq: segment.seq,
                    no,
                    byte: done,
                };
                if !self.take(&record, at, chunk, each)? {
                    return Ok(false);
                }
                done += take as u64;
            }
            self.next.no += 1;
        }
        Ok(true)
    }

    /// What is left of each stream's unfinished line, once the container has ended.
    pub fn finish(&mut self, each: &mut dyn FnMut(Piece<'_>) -> Flow) -> Flow {
        for (s, open) in self.open.iter_mut().enumerate() {
            if let Some(line) = open.take()
                && (!line.started || !line.held.is_empty())
            {
                let piece = Piece {
                    stream: stream_of(s),
                    at: line.at,
                    first: !line.started,
                    bytes: &line.held,
                };
                if !each(piece)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Takes `bytes` of `record`, the first at `pos`.
    fn take(
        &mut self,
        record: &Record,
        pos: Pos,
        mut bytes: &[u8],
        each: &mut dyn FnMut(Piece<'_>) -> Flow,
    ) -> Flow {
        let s = record.stream;
        let mut at = pos;
        while !bytes.is_empty() {
            let (piece, rest) = match bytes.iter().position(|&b| b == b'\n') {
                Some(end) => bytes.split_at(end + 1),
                None => (bytes, &[][..]),
            };
            let slot = self
                .open
                .get_mut(s)
                .ok_or_else(|| io::Error::other("no such stream"))?;
            // A line starting before where this reader starts is not its to show.
            if slot.is_none() && at >= self.from.get(s).copied().unwrap_or(NEVER) {
                *slot = Some(Open {
                    at: record.at,
                    started: false,
                    held: Vec::new(),
                });
            }
            if let Some(line) = slot {
                line.held.extend_from_slice(piece);
                let ended = piece.ends_with(b"\n");
                if ended || line.held.len() >= PIECE {
                    let out = Piece {
                        stream: stream_of(s),
                        at: line.at,
                        first: !line.started,
                        bytes: &line.held,
                    };
                    let go_on = each(out)?;
                    line.started = true;
                    line.held.clear();
                    if ended {
                        *slot = None;
                    }
                    if !go_on {
                        return Ok(false);
                    }
                }
            }
            at.byte += piece.len() as u64;
            bytes = rest;
        }
        Ok(true)
    }
}

fn stream_of(slot: usize) -> u8 {
    if slot == 1 { LOG_STDERR } else { LOG_STDOUT }
}

/// Reads a whole file into `out`, for tests.
#[cfg(test)]
fn read_all(path: &Path) -> Vec<u8> {
    use std::io::Read as _;
    let mut out = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut out))
        .unwrap_or_default();
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shards-logs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes records as the workload's `Logger` does, with or without an index.
    fn write(dir: &Path, records: &[(u8, u64, Vec<u8>)], indexed: bool) {
        write_segment(dir, 0, records, indexed);
    }

    /// Writes records as segment `seq`.
    fn write_segment(dir: &Path, seq: u64, records: &[(u8, u64, Vec<u8>)], indexed: bool) {
        let (log_name, index_name) = log_segment(seq);
        let mut log = Vec::new();
        let mut index = Vec::new();
        for (stream, at, bytes) in records {
            // The writer keeps no empty record; an earlier shards did.
            if bytes.is_empty() && indexed {
                continue;
            }
            let mut entry = log.len() as u64;
            if *stream == LOG_STDERR {
                entry |= INDEX_STDERR;
            }
            if bytes.last() == Some(&b'\n') {
                entry |= INDEX_LINE;
            }
            log.push(*stream);
            log.extend(at.to_be_bytes());
            log.extend(u32::try_from(bytes.len()).unwrap().to_be_bytes());
            log.extend(bytes);
            index.extend(entry.to_be_bytes());
        }
        std::fs::write(dir.join(log_name), log).unwrap();
        if indexed {
            std::fs::write(dir.join(index_name), index).unwrap();
        } else {
            let _ = std::fs::remove_file(dir.join(index_name));
        }
    }

    /// Writes records in segments as the workload's `Logger` does with `size` and
    /// `files`, and removes those `drop` more of the oldest; returns the records of the
    /// segments left, and how many segments there were.
    fn write_segments(
        dir: &Path,
        records: &[(u8, u64, Vec<u8>)],
        size: u64,
        files: u64,
        drop: u64,
    ) -> (Vec<(u8, u64, Vec<u8>)>, u64) {
        let mut segments: Vec<Vec<(u8, u64, Vec<u8>)>> = vec![Vec::new()];
        let mut logged = 0u64;
        for record in records.iter().filter(|r| !r.2.is_empty()) {
            let len = LOG_HEAD + record.2.len() as u64;
            if logged > 0 && logged + len > size {
                segments.push(Vec::new());
                logged = 0;
            }
            logged += len;
            segments.last_mut().unwrap().push(record.clone());
        }
        let total = segments.len() as u64;
        let first = total.saturating_sub(files).saturating_add(drop).min(total - 1);
        // What is written again is written in place, as a writer's appends are.
        let written: Vec<String> = (first..total)
            .flat_map(|seq| <[String; 2]>::from(log_segment(seq)))
            .collect();
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if !written.contains(&entry.file_name().into_string().unwrap()) {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        let mut kept = Vec::new();
        for (seq, segment) in segments.iter().enumerate().skip(first as usize) {
            write_segment(dir, seq as u64, segment, true);
            kept.extend(segment.iter().cloned());
        }
        (kept, total)
    }

    /// What `logs` shows, the plain way: every line in memory, each stream's lines in the
    /// order their last bytes came, the last `n` of them all; unfinished ones only once
    /// `ended`. Each stream's bytes, and each line's time.
    fn expected(records: &[(u8, u64, Vec<u8>)], n: Option<usize>, ended: bool) -> [Vec<(u64, Vec<u8>)>; 2] {
        // (position of last byte, stream, time, bytes)
        let mut lines: Vec<(usize, usize, u64, Vec<u8>)> = Vec::new();
        let mut open: [Option<(u64, Vec<u8>, usize)>; 2] = [None, None];
        let mut pos = 0usize;
        for (stream, at, bytes) in records {
            let s = usize::from(*stream == LOG_STDERR);
            for &b in bytes {
                let line = open[s].get_or_insert((*at, Vec::new(), pos));
                line.1.push(b);
                line.2 = pos;
                if b == b'\n' {
                    let (t, bytes, last) = open[s].take().unwrap();
                    lines.push((last, s, t, bytes));
                }
                pos += 1;
            }
        }
        if ended {
            for (s, line) in open.iter_mut().enumerate() {
                if let Some((t, bytes, last)) = line.take() {
                    lines.push((last, s, t, bytes));
                }
            }
        }
        lines.sort_by_key(|l| l.0);
        let skip = n.map_or(0, |n| lines.len().saturating_sub(n));
        let mut out = [Vec::new(), Vec::new()];
        for (_, s, t, bytes) in lines.into_iter().skip(skip) {
            out[s].push((t, bytes));
        }
        out
    }

    /// What a reader gives, put back into lines: each stream's lines, each its time and
    /// bytes.
    fn shown(dir: &Path, n: Option<usize>, ended: bool) -> [Vec<(u64, Vec<u8>)>; 2] {
        let log = LogFile::open(dir).unwrap();
        let mut reader = match n {
            Some(n) => Reader::from(tail(&log, n as u64, ended).unwrap()),
            None => Reader::new(),
        };
        let mut out: [Vec<(u64, Vec<u8>)>; 2] = [Vec::new(), Vec::new()];
        let mut each = |p: Piece<'_>| {
            assert!(p.bytes.len() <= PIECE + CHUNK, "a piece past its bound");
            let s = usize::from(p.stream == LOG_STDERR);
            if p.first {
                out[s].push((p.at, Vec::new()));
            }
            out[s].last_mut().unwrap().1.extend_from_slice(p.bytes);
            Ok(true)
        };
        reader.read(&log, &mut each).unwrap();
        if ended {
            reader.finish(&mut each).unwrap();
        }
        out
    }

    /// A random log: lines of every length about a piece, on both streams, in records
    /// split anywhere, some ending mid-line.
    fn random(seed: u64) -> Vec<(u8, u64, Vec<u8>)> {
        let mut x = seed | 1;
        let mut next = move |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        let mut records = Vec::new();
        let mut at = 1_000;
        for _ in 0..next(40) {
            let stream = if next(3) == 0 { LOG_STDERR } else { LOG_STDOUT };
            let len = match next(4) {
                0 => next(4) as usize,
                1 => next(200) as usize,
                2 => PIECE - 2 + next(5) as usize,
                _ => next(3 * PIECE as u64) as usize,
            };
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    if next(30) == 0 {
                        b'\n'
                    } else {
                        b'a' + next(26) as u8
                    }
                })
                .collect();
            at += 1 + next(1000);
            records.push((stream, at, bytes));
        }
        records
    }

    /// Every way of reading a log shows what the plain way shows: whole or its last n
    /// lines, the container ended or not, indexed or from an earlier shards (audit A12).
    #[test]
    fn logs_read_as_the_plain_way_reads_them() {
        let dir = temp("differential");
        for seed in 1..400u64 {
            let records = random(seed);
            for indexed in [true, false] {
                write(&dir, &records, indexed);
                for n in [None, Some(0), Some(1), Some(2), Some(3), Some(7), Some(1000)] {
                    for ended in [true, false] {
                        let (got, want) = (shown(&dir, n, ended), expected(&records, n, ended));
                        if got != want {
                            let lens = |x: &[Vec<(u64, Vec<u8>)>; 2]| {
                                x.iter()
                                    .map(|l| l.iter().map(|(t, b)| (*t, b.len())).collect::<Vec<_>>())
                                    .collect::<Vec<_>>()
                            };
                            let (g, w) = (lens(&got), lens(&want));
                            let first = |s: usize| {
                                let i = g[s].iter().zip(&w[s]).position(|(a, b)| a != b);
                                (g[s].len(), w[s].len(), i, i.map(|i| (g[s].get(i), w[s].get(i))))
                            };
                            panic!(
                                "seed {seed}, indexed {indexed}, tail {n:?}, ended {ended}\nstdout {:?}\nstderr {:?}\nrecords {:?}",
                                first(0),
                                first(1),
                                records
                                    .iter()
                                    .map(|(s, t, b)| (*s, *t, b.len(), b.last() == Some(&b'\n')))
                                    .collect::<Vec<_>>()
                            );
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A log in segments reads as the records of those still there, in order, whole or
    /// its last n lines, the container ended or not, whatever segments are gone (audit
    /// A12).
    #[test]
    fn logs_in_segments_read_as_their_records_do() {
        let dir = temp("segments");
        for seed in 1..300u64 {
            let records = random(seed);
            let size = [1, 100, 2_000, PIECE as u64, 5 * PIECE as u64][seed as usize % 5];
            for (files, drop) in [(u64::MAX, 0), (3, 0), (2, 1), (1, 0)] {
                let (kept, total) = write_segments(&dir, &records, size, files, drop);
                for n in [None, Some(0), Some(1), Some(3), Some(50), Some(1000)] {
                    for ended in [true, false] {
                        assert_eq!(
                            shown(&dir, n, ended),
                            expected(&kept, n, ended),
                            "seed {seed}, size {size}, {total} segments, files {files}, drop {drop}, tail {n:?}, ended {ended}"
                        );
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader following a log in segments shows each line once, whole, as it comes,
    /// across segments as they begin (audit A12).
    #[test]
    fn a_reader_following_segments_shows_each_line_once() {
        let dir = temp("follow-segments");
        for seed in 1..100u64 {
            let records = random(seed);
            let size = [1, 300, 4 * PIECE as u64][seed as usize % 3];
            let mut reader = Reader::new();
            let mut out: [Vec<(u64, Vec<u8>)>; 2] = [Vec::new(), Vec::new()];
            let mut each = |p: Piece<'_>| {
                let s = usize::from(p.stream == LOG_STDERR);
                if p.first {
                    out[s].push((p.at, Vec::new()));
                }
                out[s].last_mut().unwrap().1.extend_from_slice(p.bytes);
                Ok(true)
            };
            for upto in 0..=records.len() {
                write_segments(&dir, &records[..upto], size, u64::MAX, 0);
                let log = LogFile::open(&dir).unwrap();
                reader.read(&log, &mut each).unwrap();
            }
            reader.finish(&mut each).unwrap();
            assert_eq!(out, expected(&records, None, true), "seed {seed}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader reads on in a segment removed under it, and past those removed before it
    /// came to them, to the oldest still there.
    #[test]
    fn a_reader_reads_on_past_removed_segments() {
        let dir = temp("removed");
        let records: Vec<(u8, u64, Vec<u8>)> = (0..6u64)
            .map(|i| (LOG_STDOUT, i, format!("line {i}\n").into_bytes()))
            .collect();
        // One record a segment.
        let (_, total) = write_segments(&dir, &records[..2], 1, u64::MAX, 0);
        assert_eq!(total, 2);
        let log = LogFile::open(&dir).unwrap();
        let mut lines = Vec::new();
        let mut reader = Reader::new();
        let mut first = true;
        reader
            .read(&log, &mut |p| {
                lines.push(String::from_utf8(p.bytes.to_vec()).unwrap());
                // Its writer goes on while the first line is shown: the segment read is
                // removed, and so are the next two.
                if std::mem::take(&mut first) {
                    for seq in 2..6u64 {
                        write_segment(&dir, seq, &records[seq as usize..=seq as usize], true);
                    }
                    for seq in 0..4u64 {
                        let (l, i) = log_segment(seq);
                        std::fs::remove_file(dir.join(i)).unwrap();
                        std::fs::remove_file(dir.join(l)).unwrap();
                    }
                }
                Ok(true)
            })
            .unwrap();
        assert_eq!(lines, ["line 0\n", "line 4\n", "line 5\n"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A segment's writer may add to it and go on to the next as a reader reads it: the
    /// reader, having asked whether it was whole before counting its records, reads what
    /// was added before going on.
    #[test]
    fn a_segment_finished_as_it_is_read_is_read_to_its_end() {
        let dir = temp("finished");
        let records: Vec<(u8, u64, Vec<u8>)> = (0..3u64)
            .map(|i| (LOG_STDOUT, i, format!("line {i}\n").into_bytes()))
            .collect();
        write_segment(&dir, 0, &records[..1], true);
        let log = LogFile::open(&dir).unwrap();
        let mut lines = Vec::new();
        let mut reader = Reader::new();
        let mut first = true;
        let mut each = |p: Piece<'_>| {
            lines.push(String::from_utf8(p.bytes.to_vec()).unwrap());
            if std::mem::take(&mut first) {
                write_segment(&dir, 0, &records[..2], true);
                write_segment(&dir, 1, &records[2..], true);
            }
            Ok(true)
        };
        reader.read(&log, &mut each).unwrap();
        reader.read(&log, &mut each).unwrap();
        assert_eq!(lines, ["line 0\n", "line 1\n", "line 2\n"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader that read a segment when it was the last goes on to the oldest segment
    /// after it still there, the next gone.
    #[test]
    fn a_reader_goes_on_past_a_next_segment_gone() {
        let dir = temp("next-gone");
        let records: Vec<(u8, u64, Vec<u8>)> = (0..4u64)
            .map(|i| (LOG_STDOUT, i, format!("line {i}\n").into_bytes()))
            .collect();
        write_segment(&dir, 0, &records[..1], true);
        let log = LogFile::open(&dir).unwrap();
        let mut lines = Vec::new();
        let mut each = |p: Piece<'_>| {
            lines.push(String::from_utf8(p.bytes.to_vec()).unwrap());
            Ok(true)
        };
        let mut reader = Reader::new();
        reader.read(&log, &mut each).unwrap();
        for seq in 1..4u64 {
            write_segment(&dir, seq, &records[seq as usize..=seq as usize], true);
        }
        for seq in 0..2u64 {
            let (l, i) = log_segment(seq);
            std::fs::remove_file(dir.join(i)).unwrap();
            std::fs::remove_file(dir.join(l)).unwrap();
        }
        reader.read(&log, &mut each).unwrap();
        assert_eq!(lines, ["line 0\n", "line 2\n", "line 3\n"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--tail` reads back only through the segments its lines are in: one before them,
    /// damaged, is not read.
    #[test]
    fn a_tail_reads_only_the_segments_of_its_lines() {
        let dir = temp("tail-segments");
        let records: Vec<(u8, u64, Vec<u8>)> = (0..6u64)
            .map(|i| (LOG_STDERR - (i % 2) as u8, i, format!("line {i}\n").into_bytes()))
            .collect();
        write_segment(&dir, 0, &records[..2], true);
        write_segment(&dir, 1, &records[2..], true);
        std::fs::remove_file(dir.join(LOG)).unwrap();
        let log = LogFile::open(&dir).unwrap();
        assert!(Reader::new().read(&log, &mut |_| Ok(true)).is_err());
        let got = shown(&dir, Some(2), true);
        assert_eq!(got[0], vec![(5, b"line 5\n".to_vec())]);
        assert_eq!(got[1], vec![(4, b"line 4\n".to_vec())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A segment removed between the opening of its index and of its log is gone, as its
    /// writer removes it; a log missing beside its index is damage.
    #[test]
    fn a_segment_removed_as_it_is_opened_is_gone() {
        let dir = temp("removed-opening");
        write_segment(&dir, 0, &[(LOG_STDOUT, 1, b"a\n".to_vec())], true);
        write_segment(&dir, 1, &[(LOG_STDOUT, 2, b"b\n".to_vec())], true);
        let log = LogFile::open(&dir).unwrap();
        let index = File::open(dir.join(INDEX)).unwrap();
        std::fs::remove_file(dir.join(LOG)).unwrap();
        let e = log.segment_of_index(0, index.try_clone().unwrap()).unwrap_err();
        assert!(e.to_string().contains("log is missing"), "{e}");
        std::fs::remove_file(dir.join(INDEX)).unwrap();
        assert!(log.segment_of_index(0, index).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader following a log shows each line once, whole, as it comes, whatever
    /// records it was read between (audit A12).
    #[test]
    fn a_reader_following_a_log_shows_each_line_once() {
        let dir = temp("follow");
        for seed in 1..100u64 {
            let records = random(seed);
            write(&dir, &[], true);
            let mut reader = Reader::new();
            let mut out: [Vec<(u64, Vec<u8>)>; 2] = [Vec::new(), Vec::new()];
            let mut each = |p: Piece<'_>| {
                let s = usize::from(p.stream == LOG_STDERR);
                if p.first {
                    out[s].push((p.at, Vec::new()));
                }
                out[s].last_mut().unwrap().1.extend_from_slice(p.bytes);
                Ok(true)
            };
            for upto in 0..=records.len() {
                write(&dir, &records[..upto], true);
                let log = LogFile::open(&dir).unwrap();
                reader.read(&log, &mut each).unwrap();
            }
            reader.finish(&mut each).unwrap();
            assert_eq!(out, expected(&records, None, true), "seed {seed}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record whose index entry is wrong is damage, not output; a record cut short at
    /// the end of a log an earlier shards wrote is left out.
    #[test]
    fn damage_is_refused_and_a_torn_end_left_out() {
        let dir = temp("damage");
        write(
            &dir,
            &[(LOG_STDOUT, 1, b"a\n".to_vec()), (LOG_STDERR, 2, b"b\n".to_vec())],
            true,
        );
        let mut index = read_all(&dir.join(INDEX));
        index[8] ^= 0x80;
        std::fs::write(dir.join(INDEX), &index).unwrap();
        let log = LogFile::open(&dir).unwrap();
        let e = Reader::new().read(&log, &mut |_| Ok(true)).unwrap_err();
        assert!(
            e.to_string().contains("damaged at record 1 of its segment 0"),
            "{e}"
        );

        write(&dir, &[(LOG_STDOUT, 1, b"a\n".to_vec())], false);
        let mut torn = read_all(&dir.join(LOG));
        torn.extend([LOG_STDOUT, 0, 0, 0]);
        std::fs::write(dir.join(LOG), &torn).unwrap();
        assert_eq!(shown(&dir, None, true)[0], vec![(1, b"a\n".to_vec())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--tail` reads back from the end, and only as far as its lines: a log damaged
    /// before them is still tailed, where reading it whole finds the damage.
    #[test]
    fn a_tail_reads_only_its_lines() {
        let dir = temp("backward");
        let mut records: Vec<(u8, u64, Vec<u8>)> = (0..1000u64)
            .map(|i| (LOG_STDOUT, i, format!("line {i}\n").into_bytes()))
            .collect();
        records.push((LOG_STDERR, 1000, b"last\n".to_vec()));
        write(&dir, &records, true);
        let mut log = read_all(&dir.join(LOG));
        // The first record's stream byte, which its index says is stdout.
        log[0] = 9;
        std::fs::write(dir.join(LOG), &log).unwrap();
        let opened = LogFile::open(&dir).unwrap();
        assert!(Reader::new().read(&opened, &mut |_| Ok(true)).is_err());
        let got = shown(&dir, Some(2), true);
        assert_eq!(got[0], vec![(999, b"line 999\n".to_vec())]);
        assert_eq!(got[1], vec![(1000, b"last\n".to_vec())]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
