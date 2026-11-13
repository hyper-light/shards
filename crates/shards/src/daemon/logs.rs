//! Reading a microVM's log (spec.rs, `LOG_STDOUT`) for `shards read logs` (audit A12):
//! through its index, in buffers of bounded size however long it is or its lines are, and
//! back from its end for `--tail`, reading only what the lines it shows hold.
//!
//! A line is held until it ends or reaches [`PIECE`] bytes, then goes out in pieces, its
//! prefix before the first: the bytes a client gets are the whole line's, as moby's copier
//! splits a long line into partial messages (daemon/logger/copier.go, `bufSize`).
//!
//! Lines are ordered by where their last byte is. A line a microVM left unfinished
//! counts, once it has ended, where its last byte is.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::Path;

use shards_vmm::platform;

use super::packed::{SegmentLog, open_log};
use crate::os::ReadAt as _;
use crate::segments::log_segment;
use crate::spec::{INDEX_GAP, INDEX_LINE, INDEX_START, INDEX_STDERR, LOG_HEAD, LOG_STDERR, LOG_STDOUT};

/// A log's first segment, and its index, in its microVM's directory.
pub const LOG: &str = "log";
pub const INDEX: &str = "log.idx";
/// How much is read at once, and sent a client in one message.
pub const CHUNK: usize = 64 << 10;
/// How much of a line is held before it goes out in pieces: moby's copier's buffer.
pub const PIECE: usize = 16 << 10;
/// A gap's record's stream byte, in a member's log its daemon keeps (D240; spec.rs,
/// `INDEX_GAP`): its channel's output not kept there.
pub const LOG_GAP: u8 = 3;
/// A gap's bytes: why (`control::exec::Gapped`'s byte), then how many bytes it lost
/// (big-endian u64).
pub const GAP_BYTES: usize = 1 + 8;
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
    pub const START: Pos = Pos {
        seq: 0,
        no: 0,
        byte: 0,
    };

    /// The first place of segment `seq`.
    pub fn segment_start(seq: u64) -> Pos {
        Pos { seq, ..Pos::START }
    }

    /// Its segment.
    pub fn seq(self) -> u64 {
        self.seq
    }

    /// Its bytes, as a shipper keeps its place (D163): its three fields, big-endian.
    pub fn to_bytes(self) -> [u8; 24] {
        let mut b = [0u8; 24];
        for (field, n) in b
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip([self.seq, self.no, self.byte])
        {
            field.copy_from_slice(&n.to_be_bytes());
        }
        b
    }

    /// The place [`to_bytes`](Self::to_bytes) wrote.
    pub fn from_bytes(b: &[u8; 24]) -> Pos {
        let field = |i: usize| {
            b.get(i * 8..i * 8 + 8)
                .and_then(|f| <[u8; 8]>::try_from(f).ok())
                .map_or(0, u64::from_be_bytes)
        };
        Pos {
            seq: field(0),
            no: field(1),
            byte: field(2),
        }
    }
}

/// A record: its stream (0 for stdout, 1 for stderr), when its output came, and where the
/// output is in its segment's log; or, a gap's (`gap`), where its bytes are.
#[derive(Debug, Clone, Copy)]
struct Record {
    stream: usize,
    at: u64,
    start: u64,
    len: u64,
    gap: bool,
}

/// A microVM's log: its directory, where its segments are (segments.rs),
/// held open wherever it goes.
#[derive(Debug)]
pub struct LogFile {
    dir: File,
}

/// A segment of a log, opened: read on, whatever its writer removes since, its log as
/// written or compressed (packed.rs).
#[derive(Debug)]
pub struct Segment {
    seq: u64,
    log: SegmentLog,
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

    /// Its oldest segment there: none where its last has gone too.
    pub fn oldest(&self) -> io::Result<Option<u64>> {
        Ok(self.segments()?.first().copied())
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
        match open_log(&self.dir, seq) {
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

    /// Record `no`, checked against its entry ([`check`](Self::check)).
    fn record(&self, no: u64) -> io::Result<Record> {
        let entry = self.entry(no)?;
        let mut head = [0u8; LOG_HEAD as usize];
        self.log.read_exact_at(&mut head, entry & INDEX_START)?;
        self.check(no, entry, head, self.log.len().unwrap_or(0))
    }

    /// Record `no`, its entry `entry` and its head `head`, checked against the log, `len`
    /// bytes long: a record that is not where its index says, or not whole, is damage,
    /// not output.
    fn check(&self, no: u64, entry: u64, head: [u8; LOG_HEAD as usize], len: u64) -> io::Result<Record> {
        let at = entry & INDEX_START;
        let log_len = len;
        let [stream, t @ .., l0, l1, l2, l3] = head;
        let damaged = || self.damaged(no);
        let gap = entry & INDEX_GAP != 0;
        let expected = match (gap, entry & INDEX_STDERR != 0) {
            (true, _) => LOG_GAP,
            (false, true) => LOG_STDERR,
            (false, false) => LOG_STDOUT,
        };
        if stream != expected {
            return Err(damaged());
        }
        let len = u64::from(u32::from_be_bytes([l0, l1, l2, l3]));
        if gap && len != GAP_BYTES as u64 {
            return Err(damaged());
        }
        let start = at.checked_add(LOG_HEAD).ok_or_else(damaged)?;
        if start.checked_add(len).is_none_or(|end| end > log_len) {
            return Err(damaged());
        }
        Ok(Record {
            stream: usize::from(entry & INDEX_STDERR != 0),
            at: u64::from_be_bytes(t),
            start,
            len,
            gap,
        })
    }

    /// Record `no` is damage.
    fn damaged(&self, no: u64) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the log is damaged at record {no} of its segment {}", self.seq),
        )
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
pub(super) fn segment_of(name: &str) -> Option<u64> {
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
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    shards_vmm::platform::sync_durable(&file)?;
    std::fs::rename(&aside, dir.join(INDEX))
}

/// Where `--tail n` starts, for each stream: the first byte of the first of the last `n`
/// lines of it, or the log's end, which lines to come are past. With `ended`, the lines a
/// microVM left unfinished count; without, they are still to come, and are read from
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
            // A gap's record is no output (D240).
            if entry & INDEX_GAP != 0 {
                continue;
            }
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
    /// Where its first byte is.
    start: Pos,
}

/// Where a reader of the log is: the next record, the segment it is read from, and each
/// stream's line in progress. Lines run on from one segment into the next; those whose
/// segments are gone before it reads them are gone from what it shows: the log keeps
/// its newest output alone, within its history (D186).
#[derive(Debug)]
pub struct Reader {
    next: Pos,
    segment: Option<Segment>,
    from: [Pos; 2],
    open: [Option<Open>; 2],
    /// What its reads of the log go into, and of the index: made once, however often it
    /// reads (review 7.10).
    buf: Vec<u8>,
    entries: Vec<u8>,
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
            buf: vec![0u8; CHUNK],
            entries: vec![0u8; CHUNK],
        }
    }

    /// The segment read last, to watch for more of it.
    pub fn segment(&self) -> Option<&Segment> {
        self.segment.as_ref()
    }

    /// Where a reader would start that goes on from here, for each stream (D163): where
    /// its line in progress starts, or past what this one has read.
    pub fn resume_at(&self) -> [Pos; 2] {
        let mut at = [self.next; 2];
        for ((at, open), from) in at.iter_mut().zip(&self.open).zip(self.from) {
            *at = match open {
                Some(line) => line.start,
                None => (*at).max(from),
            };
        }
        at
    }

    /// Reads the records whole so far, and gives each line's pieces to `each` as they
    /// are ready. Whether to go on: `each` may say no.
    pub fn read(&mut self, log: &LogFile, each: &mut dyn FnMut(Piece<'_>) -> Flow) -> Flow {
        let mut buf = std::mem::take(&mut self.buf);
        let mut entries = std::mem::take(&mut self.entries);
        let read = self.read_with(log, &mut buf, &mut entries, each);
        (self.buf, self.entries) = (buf, entries);
        read
    }

    fn read_with(
        &mut self,
        log: &LogFile,
        buf: &mut [u8],
        entries: &mut [u8],
        each: &mut dyn FnMut(Piece<'_>) -> Flow,
    ) -> Flow {
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
                .and_then(|whole| Ok((whole, self.read_segment(&segment, buf, entries, each)?)));
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

    /// Reads `segment`'s records whole so far, from the next, as [`walk`] reads them: each
    /// record's output taken into its stream's lines; a gap's record, no output, passed over.
    fn read_segment(
        &mut self,
        segment: &Segment,
        buf: &mut [u8],
        entries: &mut [u8],
        each: &mut dyn FnMut(Piece<'_>) -> Flow,
    ) -> Flow {
        let seq = segment.seq;
        let mut no = self.next.no;
        let read = walk(segment, &mut no, buf, entries, &mut |no, record, byte, bytes| {
            if record.gap {
                return Ok(true);
            }
            self.take(record, Pos { seq, no, byte }, bytes, each)
        });
        self.next.no = no;
        read
    }

    /// What is left of each stream's unfinished line, once the microVM has ended.
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
                    start: at,
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

/// What a log keeps, as a log lane reads it (D238): output as written, a gap's record of
/// output not kept (D240), or the segments its retention let go before it was read.
#[derive(Debug)]
pub enum Kept<'a> {
    /// Bytes of its stream's output ([`LOG_STDOUT`] or [`LOG_STDERR`]), the first at `pos`,
    /// come at `at` (nanoseconds since the Unix epoch).
    Output {
        stream: u8,
        at: u64,
        pos: Pos,
        bytes: &'a [u8],
    },
    /// `lost` bytes of its stream's output not kept there, said at `at`, and why (a byte,
    /// `control::exec::Gapped`'s): the gap's own record at `pos`.
    Gap {
        stream: u8,
        at: u64,
        pos: Pos,
        lost: u64,
        why: u8,
    },
    /// What it was to read next went with segments the log's retention let go before it read
    /// them: it reads on from `oldest`, the first place kept.
    Rotated { oldest: Pos },
}

/// A log's records as written, for a log lane (D238): each stream it reads from its place on,
/// byte for byte, in the log's order, its output in pieces of at most its lane's room. It
/// holds no line, as [`Reader`] does: a lane goes on from any byte.
#[derive(Debug)]
pub struct Records {
    next: Pos,
    segment: Option<Segment>,
    /// Each stream's place: what is before it is not given.
    from: [Pos; 2],
    reads: [bool; 2],
    piece: usize,
    buf: Vec<u8>,
    entries: Vec<u8>,
}

impl Records {
    /// A reader of the streams `reads` says, stdout's then stderr's, each from its place in
    /// `from`, its output in pieces of at most `piece` bytes (one at least).
    pub fn new(from: [Pos; 2], reads: [bool; 2], piece: usize) -> Records {
        let first = from
            .iter()
            .zip(reads)
            .filter_map(|(at, read)| read.then_some(*at))
            .min()
            .unwrap_or(Pos::START);
        Records {
            next: Pos { byte: 0, ..first },
            segment: None,
            from,
            reads,
            piece: piece.max(1),
            buf: vec![0u8; CHUNK],
            entries: vec![0u8; CHUNK],
        }
    }

    /// The segment read last, to watch for more of it.
    pub fn segment(&self) -> Option<&Segment> {
        self.segment.as_ref()
    }

    /// Reads the records whole so far, giving each to `each`: whether to go on.
    pub fn read(&mut self, log: &LogFile, each: &mut dyn FnMut(Kept<'_>) -> Flow) -> Flow {
        let mut buf = std::mem::take(&mut self.buf);
        let mut entries = std::mem::take(&mut self.entries);
        let read = self.read_with(log, &mut buf, &mut entries, each);
        (self.buf, self.entries) = (buf, entries);
        read
    }

    fn read_with(
        &mut self,
        log: &LogFile,
        buf: &mut [u8],
        entries: &mut [u8],
        each: &mut dyn FnMut(Kept<'_>) -> Flow,
    ) -> Flow {
        loop {
            if self.segment.as_ref().map(|s| s.seq) != Some(self.next.seq) {
                let Some(segment) = log.first_from(self.next.seq)? else {
                    return Ok(true);
                };
                if segment.seq != self.next.seq {
                    // Its next went with segments let go before it read them: said, and read
                    // on from the oldest kept, which is past every place it was given.
                    let oldest = Pos::segment_start(segment.seq);
                    self.next = oldest;
                    if !each(Kept::Rotated { oldest })? {
                        return Ok(false);
                    }
                }
                self.segment = Some(segment);
            }
            let Some(segment) = self.segment.take() else {
                return Ok(true);
            };
            // Whether it is whole is asked before its records are counted: records it gets
            // after the count are only before a later segment's.
            let read = log
                .has_after(segment.seq)
                .and_then(|whole| Ok((whole, self.read_segment(&segment, buf, entries, each)?)));
            let seq = segment.seq;
            self.segment = Some(segment);
            match read? {
                (_, false) => return Ok(false),
                (false, true) => return Ok(true),
                (true, true) => {}
            }
            self.next = Pos::segment_start(seq.saturating_add(1));
        }
    }

    /// Gives `each` what `segment` holds of the streams it reads, from each's place: a gap at
    /// or past its place, and output's bytes at or past it, in pieces.
    fn read_segment(
        &mut self,
        segment: &Segment,
        buf: &mut [u8],
        entries: &mut [u8],
        each: &mut dyn FnMut(Kept<'_>) -> Flow,
    ) -> Flow {
        let seq = segment.seq;
        let (from, reads, piece) = (self.from, self.reads, self.piece);
        let mut no = self.next.no;
        let read = walk(segment, &mut no, buf, entries, &mut |no, record, byte, bytes| {
            if !reads.get(record.stream).copied().unwrap_or(false) {
                return Ok(true);
            }
            let from = from.get(record.stream).copied().unwrap_or(NEVER);
            let stream = stream_of(record.stream);
            let pos = Pos { seq, no, byte };
            if record.gap {
                let (why, lost) = gap_of(bytes).ok_or_else(|| segment.damaged(no))?;
                if pos < from {
                    return Ok(true);
                }
                return each(Kept::Gap {
                    stream,
                    at: record.at,
                    pos,
                    lost,
                    why,
                });
            }
            // What of these bytes is before its stream's place.
            let skip = match (seq, no).cmp(&(from.seq, from.no)) {
                std::cmp::Ordering::Less => bytes.len(),
                std::cmp::Ordering::Equal => usize::try_from(from.byte.saturating_sub(byte))
                    .map_or(bytes.len(), |n| n.min(bytes.len())),
                std::cmp::Ordering::Greater => 0,
            };
            let mut at = byte.saturating_add(skip as u64);
            for part in bytes.get(skip..).unwrap_or_default().chunks(piece) {
                let given = Kept::Output {
                    stream,
                    at: record.at,
                    pos: Pos { seq, no, byte: at },
                    bytes: part,
                };
                if !each(given)? {
                    return Ok(false);
                }
                at = at.saturating_add(part.len() as u64);
            }
            Ok(true)
        });
        self.next.no = no;
        read
    }
}

/// A gap's bytes: why, and how many bytes it lost; none for any other.
fn gap_of(bytes: &[u8]) -> Option<(u8, u64)> {
    let (why, lost) = bytes.split_first()?;
    Some((*why, u64::from_be_bytes(<[u8; 8]>::try_from(lost).ok()?)))
}

/// What [`walk`] gives each record to: its number, the record, the offset in its output of the
/// bytes given, and those bytes; whether to go on.
type EachRecord<'a> = dyn FnMut(u64, &Record, u64, &[u8]) -> Flow + 'a;

/// Walks `segment`'s records whole so far, from record `*no` on, giving `each` each one's
/// number, the record, the offset in its output of the bytes given and those bytes: their
/// entries a chunk at a time, and the records, from the first not given, as much of the log as
/// `buf` holds at a time; each whose head and bytes lie in it is given from it, and one larger
/// given in pieces. A record and an entry each read alone cost three reads and a `stat` a
/// record, of a log of short lines (review 7.10, PM M95). `*no` is left at the first record
/// not given whole: one `each` stopped at, or past the last.
fn walk(
    segment: &Segment,
    no: &mut u64,
    buf: &mut [u8],
    entries: &mut [u8],
    each: &mut EachRecord<'_>,
) -> Flow {
    let count = segment.count()?;
    // The records counted are whole in it: their writer writes a record, then its
    // entry.
    let log_len = segment.log.len()?;
    // At least one, so that each pass takes one or fails.
    let per = (entries.len() / 8).max(1) as u64;
    while *no < count {
        let n = (count - *no).min(per);
        let raw = entries
            .get_mut(..usize::try_from(n * 8).unwrap_or(0))
            .unwrap_or_default();
        segment.index.read_exact_at(raw, *no * 8)?;
        let entry = |i: usize| {
            raw.get(i * 8..i * 8 + 8)
                .and_then(|e| e.try_into().ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| io::Error::other("an index entry out of its chunk"))
        };
        let n = usize::try_from(n).unwrap_or(0);
        let mut i = 0;
        while i < n {
            let first = entry(i)? & INDEX_START;
            let span = usize::try_from(log_len.saturating_sub(first).min(buf.len() as u64)).unwrap_or(0);
            let held = buf.get_mut(..span).unwrap_or_default();
            segment.log.read_exact_at(held, first)?;
            let mut taken = 0;
            while i < n {
                let e = entry(i)?;
                // Where in what is held it starts: none before, as entries go on.
                let Some(rel) = (e & INDEX_START)
                    .checked_sub(first)
                    .and_then(|r| usize::try_from(r).ok())
                else {
                    break;
                };
                let Some(head) = held
                    .get(rel..rel + LOG_HEAD as usize)
                    .and_then(|h| <[u8; LOG_HEAD as usize]>::try_from(h).ok())
                else {
                    if taken == 0 {
                        // Not even its head in the rest of the log.
                        return Err(segment.damaged(*no));
                    }
                    break;
                };
                let record = segment.check(*no, e, head, log_len)?;
                let out = rel + LOG_HEAD as usize;
                let output = usize::try_from(record.len)
                    .ok()
                    .and_then(|len| out.checked_add(len))
                    .and_then(|end| held.get(out..end));
                match output {
                    Some(output) => {
                        if !each(*no, &record, 0, output)? {
                            return Ok(false);
                        }
                    }
                    // Larger than the buffer: alone, in pieces.
                    None if taken == 0 => {
                        if !in_pieces(segment, &record, *no, held, each)? {
                            return Ok(false);
                        }
                    }
                    // Read again, from it.
                    None => break,
                }
                *no += 1;
                i += 1;
                taken += 1;
            }
        }
    }
    Ok(true)
}

/// Gives `each` `record`, number `no`, larger than `buf`, a piece at a time.
fn in_pieces(segment: &Segment, record: &Record, no: u64, buf: &mut [u8], each: &mut EachRecord<'_>) -> Flow {
    let mut done = 0u64;
    while done < record.len {
        let take = usize::try_from((record.len - done).min(buf.len() as u64)).unwrap_or(0);
        let chunk = buf.get_mut(..take).unwrap_or_default();
        segment.log.read_exact_at(chunk, record.start + done)?;
        if !each(no, record, done, chunk)? {
            return Ok(false);
        }
        done += take as u64;
    }
    Ok(true)
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

    /// A directory of its own, removed when dropped, whether its test passes or panics.
    fn temp(tag: &str) -> shards_testdir::TempDir {
        shards_testdir::TempDir::new(&format!("logs-{tag}")).unwrap()
    }

    /// Writes records as the workload's `Logger` does, with or without an index.
    fn write(dir: &Path, records: &[(u8, u64, Vec<u8>)], indexed: bool) {
        write_segment(dir, 0, records, indexed);
    }

    /// `records` as a log's bytes and its index's, the log starting at offset `base`.
    fn encode(records: &[(u8, u64, Vec<u8>)], base: u64, indexed: bool) -> (Vec<u8>, Vec<u8>) {
        let mut log = Vec::new();
        let mut index = Vec::new();
        for (stream, at, bytes) in records {
            // The writer keeps no empty record; an earlier shards did.
            if bytes.is_empty() && indexed {
                continue;
            }
            let mut entry = base + log.len() as u64;
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
        (log, index)
    }

    /// Appends `record` to segment 0 and its index, as the writer does.
    fn append(dir: &Path, record: &(u8, u64, Vec<u8>)) {
        use std::io::Write as _;
        let (log_name, index_name) = log_segment(0);
        let base = std::fs::metadata(dir.join(&log_name)).unwrap().len();
        let (log, index) = encode(std::slice::from_ref(record), base, true);
        let open = |name: &str| {
            std::fs::OpenOptions::new()
                .append(true)
                .open(dir.join(name))
                .unwrap()
        };
        open(&log_name).write_all(&log).unwrap();
        open(&index_name).write_all(&index).unwrap();
    }

    /// Writes records as segment `seq`.
    fn write_segment(dir: &Path, seq: u64, records: &[(u8, u64, Vec<u8>)], indexed: bool) {
        let (log_name, index_name) = log_segment(seq);
        let (log, index) = encode(records, 0, indexed);
        std::fs::write(dir.join(log_name), log).unwrap();
        if indexed {
            std::fs::write(dir.join(index_name), index).unwrap();
        } else {
            let _ = std::fs::remove_file(dir.join(index_name));
        }
    }

    /// The segments of at most `size` bytes a writer keeps `records` in whole, as shards
    /// before D163 did: a record goes to the last, or begins the next. So one more record
    /// changes only the segment it goes to. Readers read logs so written, and those the
    /// workload's `Logger` now writes, its records cut to fill each segment, alike.
    fn split(records: &[(u8, u64, Vec<u8>)], size: u64) -> Vec<Vec<(u8, u64, Vec<u8>)>> {
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
        segments
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
        let segments = split(records, size);
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
            let len = match next(20) {
                0..5 => next(4) as usize,
                5..10 => next(200) as usize,
                10..15 => PIECE - 2 + next(5) as usize,
                // Now and then, larger than what a read holds at once.
                15 => CHUNK + next(CHUNK as u64) as usize,
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
    /// lines, the microVM ended or not, indexed or from an earlier shards (audit A12).
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
    /// its last n lines, the microVM ended or not, whatever segments are gone (audit
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

    /// A log whose rotated segments are compressed (D162) reads as their records do, as
    /// one kept as written does: whole or its last n lines, the microVM ended or not.
    #[test]
    fn logs_in_packed_segments_read_as_their_records_do() {
        let dir = temp("packed");
        for seed in 1..60u64 {
            let records = random(seed);
            let size = [1, 100, 2_000, PIECE as u64, 5 * PIECE as u64][seed as usize % 5];
            for (files, drop) in [(u64::MAX, 0), (3, 0)] {
                let (kept, total) = write_segments(&dir, &records, size, files, drop);
                let open = platform::open_dir(&dir).unwrap();
                let first = total.saturating_sub(files.min(total));
                for seq in first..total.saturating_sub(1) {
                    assert!(super::super::packed::pack_for_test(&open, seq).unwrap());
                }
                for n in [None, Some(0), Some(1), Some(3), Some(50), Some(1000)] {
                    for ended in [true, false] {
                        assert_eq!(
                            shown(&dir, n, ended),
                            expected(&kept, n, ended),
                            "seed {seed}, size {size}, {total} segments, files {files}, tail {n:?}, ended {ended}"
                        );
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader started where another got to (`resume_at`, D163), as a log is written on,
    /// shows each line the other did not, from its start: together they show every line
    /// once, whatever the segments and wherever the first stopped.
    #[test]
    fn a_reader_resumed_where_another_got_to_shows_the_rest() {
        let dir = temp("resumed");
        for seed in 1..200u64 {
            // Lines shorter than a piece, which a reader never shows in part before they
            // end: what it holds of one is what the next reader shows whole.
            let records = random(seed);
            if expected(&records, None, true)
                .iter()
                .flatten()
                .any(|(_, line)| line.len() >= PIECE)
            {
                continue;
            }
            let size = [1, 100, 2_000, 5 * PIECE as u64][seed as usize % 4];
            let cut = records.len() / 2;
            let mut out: [Vec<(u64, Vec<u8>)>; 2] = [Vec::new(), Vec::new()];
            fn gather(out: &mut [Vec<(u64, Vec<u8>)>; 2]) -> impl FnMut(Piece<'_>) -> Flow + '_ {
                |p: Piece<'_>| {
                    let s = usize::from(p.stream == LOG_STDERR);
                    if p.first {
                        out[s].push((p.at, Vec::new()));
                    }
                    out[s].last_mut().unwrap().1.extend_from_slice(p.bytes);
                    Ok(true)
                }
            }
            write_segments(&dir, &records[..cut], size, u64::MAX, 0);
            let mut first = Reader::new();
            first
                .read(&LogFile::open(&dir).unwrap(), &mut gather(&mut out))
                .unwrap();
            let at = first.resume_at();
            write_segments(&dir, &records, size, u64::MAX, 0);
            let at = at.map(|p| Pos::from_bytes(&p.to_bytes()));
            let mut next = Reader::from(at);
            next.read(&LogFile::open(&dir).unwrap(), &mut gather(&mut out))
                .unwrap();
            next.finish(&mut gather(&mut out)).unwrap();
            assert_eq!(
                out,
                expected(&records, None, true),
                "seed {seed}, size {size}, cut {cut}"
            );
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
            write_segments(&dir, &[], size, u64::MAX, 0);
            for upto in 0..=records.len() {
                // Each record rewrites only the segment it goes to, as the writer's
                // appends do: rewriting every segment each time spent the test's time
                // in the file system's metadata calls, not in the reader.
                let segments = split(&records[..upto], size);
                let last = segments.len() - 1;
                write_segment(&dir, last as u64, &segments[last], true);
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
                // Each record appended as the writer appends it, the log read again.
                if let Some(record) = upto.checked_sub(1).and_then(|i| records.get(i)) {
                    append(&dir, record);
                }
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

        // An entry past the log's end.
        write(&dir, &[(LOG_STDOUT, 1, b"a\n".to_vec())], true);
        let mut index = read_all(&dir.join(INDEX));
        index[7] = 0x40;
        std::fs::write(dir.join(INDEX), &index).unwrap();
        let log = LogFile::open(&dir).unwrap();
        let e = Reader::new().read(&log, &mut |_| Ok(true)).unwrap_err();
        assert!(
            e.to_string().contains("damaged at record 0 of its segment 0"),
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

    /// A writer of the log in `dir`, as the daemon keeps a member's (D240): segments of
    /// `size` bytes, as many as `files`.
    fn writer(dir: &Path, size: u64, files: u64) -> crate::log_writer::Writer {
        let newest = crate::daemon::newest_log(dir).unwrap();
        let held = platform::open_dir(dir).unwrap();
        crate::log_writer::Writer::new(
            newest.log,
            newest.index,
            newest.seq,
            crate::spec::LogRetention { size, files },
            Box::new(move |seq| crate::segments::new_segment(&held, seq, files)),
        )
        .unwrap()
    }

    /// A gap's record of `lost` bytes of `stream`, why `why`, as the daemon's keeper keeps one.
    fn gap(writer: &mut crate::log_writer::Writer, stream: u8, lost: u64, why: u8) {
        let mut bytes = vec![why];
        bytes.extend_from_slice(&lost.to_be_bytes());
        let flags = INDEX_GAP | if stream == LOG_STDERR { INDEX_STDERR } else { 0 };
        assert!(writer.keep_whole(LOG_GAP, flags, &bytes));
    }

    /// What a [`Records`] gives, owned: output as (stream, place, bytes), a gap as its stream,
    /// place, bytes lost and why, a rotation as its oldest place.
    #[derive(Debug, PartialEq, Eq)]
    enum Got {
        Output(u8, Pos, Vec<u8>),
        Gap(u8, Pos, u64, u8),
        Rotated(Pos),
    }

    fn records(dir: &Path, records: &mut Records) -> Vec<Got> {
        let log = LogFile::open(dir).unwrap();
        let mut got = Vec::new();
        records
            .read(&log, &mut |kept| {
                got.push(match kept {
                    Kept::Output {
                        stream, pos, bytes, ..
                    } => Got::Output(stream, pos, bytes.to_vec()),
                    Kept::Gap {
                        stream,
                        pos,
                        lost,
                        why,
                        ..
                    } => Got::Gap(stream, pos, lost, why),
                    Kept::Rotated { oldest } => Got::Rotated(oldest),
                });
                Ok(true)
            })
            .unwrap();
        got
    }

    fn at(seq: u64, no: u64, byte: u64) -> Pos {
        Pos { seq, no, byte }
    }

    /// A log lane's reader (D238) gives a log's records as written, byte for byte, each at its
    /// own place, in the log's order and in pieces of its room, its records cut across three
    /// segments as its writer fills each; from a place inside a record it gives that record's
    /// rest, the other stream from its own place; and a stream it does not read it passes over.
    #[test]
    fn records_give_a_log_as_written_from_any_place() {
        let dir = temp("records");
        // Segments of 40 bytes, a record's head 13: the writer fills each, a record going on
        // in the next segment where it has no more room.
        let mut w = writer(&dir, 40, 4);
        w.keep(LOG_STDOUT, b"first line\n");
        w.keep(LOG_STDERR, b"\x00\xff no newline");
        w.keep(LOG_STDOUT, b"0123456789");
        let output = |stream, pos, bytes: &[u8]| Got::Output(stream, pos, bytes.to_vec());
        let mut whole = Records::new([Pos::START; 2], [true, true], 4);
        assert_eq!(
            records(&dir, &mut whole),
            [
                output(LOG_STDOUT, at(0, 0, 0), b"firs"),
                output(LOG_STDOUT, at(0, 0, 4), b"t li"),
                output(LOG_STDOUT, at(0, 0, 8), b"ne\n"),
                output(LOG_STDERR, at(0, 1, 0), b"\x00\xff "),
                output(LOG_STDERR, at(1, 0, 0), b"no n"),
                output(LOG_STDERR, at(1, 0, 4), b"ewli"),
                output(LOG_STDERR, at(1, 0, 8), b"ne"),
                output(LOG_STDOUT, at(1, 1, 0), b"0123"),
                output(LOG_STDOUT, at(2, 0, 0), b"4567"),
                output(LOG_STDOUT, at(2, 0, 4), b"89"),
            ]
        );
        // Read on as the log grows: what comes after alone.
        w.keep(LOG_STDERR, b"later");
        assert_eq!(
            records(&dir, &mut whole),
            [
                output(LOG_STDERR, at(2, 1, 0), b"late"),
                output(LOG_STDERR, at(2, 1, 4), b"r")
            ]
        );
        // stdout from inside its first record, stderr from inside its second.
        let mut inside = Records::new([at(0, 0, 6), at(1, 0, 8)], [true, true], 64);
        assert_eq!(
            records(&dir, &mut inside),
            [
                output(LOG_STDOUT, at(0, 0, 6), b"line\n"),
                output(LOG_STDERR, at(1, 0, 8), b"ne"),
                output(LOG_STDOUT, at(1, 1, 0), b"0123"),
                output(LOG_STDOUT, at(2, 0, 0), b"456789"),
                output(LOG_STDERR, at(2, 1, 0), b"later"),
            ]
        );
        let mut stderr = Records::new([Pos::START; 2], [false, true], 64);
        assert_eq!(
            records(&dir, &mut stderr),
            [
                output(LOG_STDERR, at(0, 1, 0), b"\x00\xff "),
                output(LOG_STDERR, at(1, 0, 0), b"no newline"),
                output(LOG_STDERR, at(2, 1, 0), b"later"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gap's record (D240) is given typed, where its stream is read and its place reached,
    /// and passed over by every reader of output: `logs` and `--tail` read the log as they
    /// read it without the gap. Segments let go before a lane's reader read them are said,
    /// and it reads on from the oldest kept.
    #[test]
    fn records_say_a_gap_and_the_segments_let_go() {
        let dir = temp("gaps");
        let mut w = writer(&dir, 1 << 20, 4);
        w.keep(LOG_STDOUT, b"before\n");
        gap(&mut w, LOG_STDOUT, 4096, 1);
        w.keep(LOG_STDERR, b"err\n");
        gap(&mut w, LOG_STDERR, 7, 2);
        w.keep(LOG_STDOUT, b"after\n");
        let mut read = Records::new([Pos::START; 2], [true, true], 64);
        assert_eq!(
            records(&dir, &mut read),
            [
                Got::Output(LOG_STDOUT, at(0, 0, 0), b"before\n".to_vec()),
                Got::Gap(LOG_STDOUT, at(0, 1, 0), 4096, 1),
                Got::Output(LOG_STDERR, at(0, 2, 0), b"err\n".to_vec()),
                Got::Gap(LOG_STDERR, at(0, 3, 0), 7, 2),
                Got::Output(LOG_STDOUT, at(0, 4, 0), b"after\n".to_vec()),
            ]
        );
        // From past the first gap's record, stdout alone: no gap of stdout's again.
        let mut past = Records::new([at(0, 2, 0), Pos::START], [true, false], 64);
        assert_eq!(
            records(&dir, &mut past),
            [Got::Output(LOG_STDOUT, at(0, 4, 0), b"after\n".to_vec())]
        );
        // `logs` and its tail: the output alone, as if no gap were there.
        let plain = temp("gaps-plain");
        let mut p = writer(&plain, 1 << 20, 4);
        for (stream, bytes) in [
            (LOG_STDOUT, &b"before\n"[..]),
            (LOG_STDERR, b"err\n"),
            (LOG_STDOUT, b"after\n"),
        ] {
            p.keep(stream, bytes);
        }
        let without_times = |shown: [Vec<(u64, Vec<u8>)>; 2]| {
            shown.map(|s| s.into_iter().map(|(_, b)| b).collect::<Vec<_>>())
        };
        assert_eq!(
            without_times(shown(&dir, None, true)),
            without_times(shown(&plain, None, true))
        );
        assert_eq!(
            without_times(shown(&dir, Some(2), true)),
            without_times(shown(&plain, Some(2), true))
        );
        // Its retention lets the first two segments go before the reader reads on.
        let rotated = temp("rotated");
        let mut r = writer(&rotated, 30, 2);
        r.keep(LOG_STDOUT, b"one\n");
        let mut follower = Records::new([Pos::START; 2], [true, true], 64);
        assert_eq!(
            records(&rotated, &mut follower),
            [Got::Output(LOG_STDOUT, at(0, 0, 0), b"one\n".to_vec())]
        );
        for word in [&b"two two two\n"[..], b"three three\n", b"four four\n"] {
            r.keep(LOG_STDOUT, word);
        }
        let got = records(&rotated, &mut follower);
        assert_eq!(got.first(), Some(&Got::Rotated(at(2, 0, 0))), "{got:?}");
        assert_eq!(
            got.last(),
            Some(&Got::Output(LOG_STDOUT, at(3, 0, 0), b"four four\n".to_vec()))
        );
        for dir in [dir, plain, rotated] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
