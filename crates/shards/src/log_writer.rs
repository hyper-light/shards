//! A log's writer (spec.rs, `LOG_STDOUT`): its records and their index in segments, as
//! its retention keeps them (segments.rs). A run's VM writes its run's log with it
//! (workload.rs), and its daemon each member's of the run (D240's drain, daemon/managed).

use std::fs;
use std::io::{self, Write};

use crate::spec::{INDEX_LINE, INDEX_START, INDEX_STDERR, LOG_HEAD, LOG_STDERR, LogRetention, now};

/// Keeps a microVM's output (spec.rs, `LOG_STDOUT`): each record appended to its log,
/// then its entry to the log's index once the record is whole, in segments kept as its
/// retention says (segments.rs). A record that cannot be kept, on a full disk
/// or a log removed, costs the log, not the run: it is taken back, so neither file holds
/// any of it, and its bytes are counted as lost (audit A12).
pub struct Writer {
    /// Makes the next segment: in a VM, its daemon, since no VM reaches a microVM's
    /// directory (D30).
    next: NextSegment,
    retention: LogRetention,
    /// The segment written, and its files.
    seq: u64,
    log: fs::File,
    index: fs::File,
    /// The log's and the index's lengths, where a record that fails is cut back to.
    logged: u64,
    indexed: u64,
    lost: u64,
}

/// Makes a log's segment `seq`, its log and index to append to, and removes the oldest
/// past the log's retention (segments.rs, `new_segment`).
pub type NextSegment = Box<dyn FnMut(u64) -> io::Result<(fs::File, fs::File)> + Send>;

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("seq", &self.seq)
            .field("logged", &self.logged)
            .field("lost", &self.lost)
            .finish_non_exhaustive()
    }
}

impl Writer {
    /// Writes a microVM's log from its segment `seq`, `log` and `index`, which its
    /// daemon made with the microVM (or its newest, for one started again), going on in
    /// the segments `next` makes.
    pub fn new(
        log: fs::File,
        index: fs::File,
        seq: u64,
        retention: LogRetention,
        next: NextSegment,
    ) -> io::Result<Writer> {
        let logged = log.metadata()?.len();
        let mut indexed = index.metadata()?.len();
        // A partial entry an earlier writer left is not one.
        if indexed % 8 != 0 {
            indexed -= indexed % 8;
            index.set_len(indexed)?;
        }
        Ok(Writer {
            next,
            retention,
            seq,
            log,
            index,
            logged,
            indexed,
            lost: 0,
        })
    }

    /// Keeps `bytes` of `stream` as records: as much as the segment written has room for,
    /// then the rest in the next, so that each segment is as long as its size and the log
    /// keeps all its retention allows. A segment's first record takes a byte at least,
    /// however small its size. Each record says whether it ends a line (INDEX_LINE), so a
    /// line cut across records, or segments, is read whole. Before, a record that did not
    /// fit went whole into the next segment: a guest's 32 KiB records left segments of
    /// 64,000 bytes half full, and a log half its retention (CI's x86_64 Linux, D162).
    pub fn keep(&mut self, stream: u8, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let room = self
                .retention
                .size
                .saturating_sub(self.logged)
                .saturating_sub(LOG_HEAD);
            let room = if self.logged == 0 { room.max(1) } else { room };
            if room == 0 {
                if self.rotate().is_err() {
                    // A log that cannot go on within its bound keeps nothing more.
                    self.lost = self.lost.saturating_add(bytes.len() as u64);
                    return;
                }
                continue;
            }
            let (part, rest) = bytes.split_at(usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len()));
            self.record(stream, part);
            bytes = rest;
        }
    }

    /// Writes `bytes` of `stream` as one record of the segment written.
    fn record(&mut self, stream: u8, bytes: &[u8]) {
        let head = log_head(stream, bytes);
        let record_len = LOG_HEAD + bytes.len() as u64;
        let mut entry = self.logged & INDEX_START;
        if stream == LOG_STDERR {
            entry |= INDEX_STDERR;
        }
        if bytes.last() == Some(&b'\n') {
            entry |= INDEX_LINE;
        }
        let kept = write_parts(&self.log, &mut [io::IoSlice::new(&head), io::IoSlice::new(bytes)])
            .and_then(|()| (&self.index).write_all(&entry.to_be_bytes()));
        match kept {
            Ok(()) => {
                self.logged += record_len;
                self.indexed += 8;
            }
            Err(_) => {
                self.lost = self.lost.saturating_add(bytes.len() as u64);
                let _ = self.index.set_len(self.indexed);
                let _ = self.log.set_len(self.logged);
            }
        }
    }

    /// Starts the next segment.
    fn rotate(&mut self) -> io::Result<()> {
        let next = self.seq + 1;
        let (log, index) = (self.next)(next)?;
        (self.seq, self.log, self.index, self.logged, self.indexed) = (next, log, index, 0, 0);
        Ok(())
    }

    /// Bytes of output not kept.
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

/// The head of a log record for `bytes` on `stream`, stamped now: its bytes follow it.
pub fn log_head(stream: u8, bytes: &[u8]) -> [u8; LOG_HEAD as usize] {
    let [t0, t1, t2, t3, t4, t5, t6, t7] = u64::try_from(now()).unwrap_or(u64::MAX).to_be_bytes();
    let [l0, l1, l2, l3] = u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes();
    [stream, t0, t1, t2, t3, t4, t5, t6, t7, l0, l1, l2, l3]
}

/// Writes `parts` in order with as few writes as they take, a short or interrupted one
/// going on from where it stopped: a log record's head and bytes, without copying the
/// bytes after their head (audit D08).
pub fn write_parts(mut w: impl Write, mut rest: &mut [io::IoSlice<'_>]) -> io::Result<()> {
    while !rest.is_empty() {
        match w.write_vectored(rest) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => io::IoSlice::advance_slices(&mut rest, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
