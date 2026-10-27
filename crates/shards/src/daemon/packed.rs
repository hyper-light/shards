//! A container log's rotated segments, compressed (D162). Once a segment's writer has
//! gone on to the next, its log is written again as zstd frames of [`FRAME`] bytes of it,
//! each compressed alone, then their seek table, as zstd's seekable format lays them out
//! (facebook/zstd contrib/seekable_format, v0.1.0): a reader reads any record of it by
//! decompressing its frame alone, and `zstd -d` reads the whole back. Its index stays as
//! written, which readers find records by (logs.rs). Done on the packer's thread, off
//! every run's path and every other file a run waits for (files.rs).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use shards_vmm::platform;

use super::{Daemon, Threads, lock, log};
use crate::containers::Disk;
use crate::os::ReadAt as _;
use crate::segments::{log_segment, packed_segment};

/// How much of a segment's log a frame holds, the most a reader decompresses to read a
/// record of it: real logs in frames of 1 MiB come to 7.63% of their size, where whole
/// they come to 7.53% and in frames of 256 KiB to 8.00%, and a frame is read back in
/// 1.7 ms at p99 (PM M231).
pub const FRAME: usize = 1 << 20;
/// klauspost's SpeedBestCompression (`EncoderLevelFromZstd(11)`): real logs at 7.6% of
/// their size, where Docker's gzip of each rotated file leaves 9.2%, at 110 to 185 ms a
/// segment of 20 MB on the packer's thread (PM M231).
const LEVEL: i32 = 11;
/// The seek table's frame (contrib/seekable_format): a skippable frame, its entries, its
/// footer.
const SKIPPABLE_MAGIC: u32 = 0x184D_2A5E;
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;
/// `Number_Of_Frames`, `Seek_Table_Descriptor`, `Seekable_Magic_Number`.
const FOOTER: usize = 9;
/// `Compressed_Size`, `Decompressed_Size`: no checksums, as each frame has its own.
const ENTRY: usize = 8;
/// `Checksum_Flag`, and the `Reserved_Bits` a reader of this version requires clear.
const CHECKSUMS: u8 = 0x80;
const RESERVED: u8 = 0x7c;
/// The most a frame of [`FRAME`] bytes compresses to (zstd's `ZSTD_COMPRESSBOUND`).
const FRAME_BOUND: usize = FRAME + (FRAME >> 8);

/// Compresses rotated segment `seq` of the log in the directory `dir` holds open, as
/// [this module](self) says, the compressed file made durable before the log as written
/// goes: a reader finds one or the other whole, the log as written first
/// ([`open_log`]). One a crash left begun is begun again. Whether there was a log as
/// written to compress.
pub fn pack(dir: &File, seq: u64) -> io::Result<bool> {
    pack_with(dir, seq, true)
}

/// [`pack`], made durable or not: tests of what readers make of compressed segments
/// skip the flushes, each milliseconds on macOS (F_FULLFSYNC).
fn pack_with(dir: &File, seq: u64, durable: bool) -> io::Result<bool> {
    let (log_name, index_name) = log_segment(seq);
    let packed_name = packed_segment(seq);
    let written = match platform::open_in(dir, &log_name) {
        Ok(log) => log,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let out = platform::create_in(dir, &packed_name)?;
    let made = write_packed(&written, &out).and_then(|()| {
        if durable {
            platform::sync_durable(&out)?;
            platform::sync_entries(dir)?;
        }
        Ok(())
    });
    if let Err(e) = made {
        let _ = platform::unlink_in(dir, &packed_name);
        return Err(e);
    }
    match platform::unlink_in(dir, &log_name) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // Its segment removed meanwhile, as the oldest past those its log keeps: what was
    // compressed goes with it.
    if let Err(e) = platform::open_in(dir, &index_name)
        && e.kind() == io::ErrorKind::NotFound
    {
        let _ = platform::unlink_in(dir, &packed_name);
    }
    Ok(true)
}

/// For the readers' tests: compresses segment `seq` in `dir` as the packer does, but for
/// its flushes.
#[cfg(test)]
pub(super) fn pack_for_test(dir: &File, seq: u64) -> io::Result<bool> {
    pack_with(dir, seq, false)
}

/// Writes `log` to `out` as frames of [`FRAME`] bytes of it, then their seek table.
fn write_packed(log: &File, mut out: &File) -> io::Result<()> {
    let len = log.metadata()?.len();
    let mut chunk = vec![0u8; FRAME];
    let mut entries = Vec::new();
    let mut at = 0u64;
    while at < len {
        let take = usize::try_from((len - at).min(FRAME as u64)).unwrap_or(FRAME);
        let part = chunk.get_mut(..take).unwrap_or_default();
        log.read_exact_at(part, at)?;
        let frame = shards_zstd::compress(Some(LEVEL), part)?;
        out.write_all(&frame)?;
        entries.push((frame.len(), take));
        at += take as u64;
    }
    out.write_all(&seek_table(&entries)?)
}

/// The seek table of frames `entries`, each its compressed and its decompressed size.
fn seek_table(entries: &[(usize, usize)]) -> io::Result<Vec<u8>> {
    let too_large = || io::Error::new(io::ErrorKind::InvalidInput, "a seek table too large");
    let size = entries
        .len()
        .checked_mul(ENTRY)
        .and_then(|n| n.checked_add(FOOTER))
        .ok_or_else(too_large)?;
    let mut table = Vec::with_capacity(8 + size);
    table.extend(SKIPPABLE_MAGIC.to_le_bytes());
    table.extend(u32::try_from(size).map_err(|_| too_large())?.to_le_bytes());
    for &(packed, len) in entries {
        table.extend(u32::try_from(packed).map_err(|_| too_large())?.to_le_bytes());
        table.extend(u32::try_from(len).map_err(|_| too_large())?.to_le_bytes());
    }
    table.extend(
        u32::try_from(entries.len())
            .map_err(|_| too_large())?
            .to_le_bytes(),
    );
    table.push(0);
    table.extend(SEEKABLE_MAGIC.to_le_bytes());
    Ok(table)
}

/// A segment's log as its readers read it: as written, or compressed.
#[derive(Debug)]
pub enum SegmentLog {
    Written(File),
    Packed(Packed),
}

impl SegmentLog {
    /// Fills `buf` from the log at `at`, failing past its end.
    pub fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()> {
        match self {
            SegmentLog::Written(file) => file.read_exact_at(buf, at),
            SegmentLog::Packed(packed) => packed.read_exact_at(buf, at),
        }
    }

    /// How long the log is, as written.
    pub fn len(&self) -> io::Result<u64> {
        match self {
            SegmentLog::Written(file) => Ok(file.metadata()?.len()),
            SegmentLog::Packed(packed) => Ok(packed.len),
        }
    }
}

/// Segment `seq`'s log in the directory `dir` holds open, as written or compressed. The
/// log as written goes only once its compressed file is whole and durable ([`pack`]), so
/// it is looked for first: with neither there, the segment is gone (NotFound).
pub fn open_log(dir: &File, seq: u64) -> io::Result<SegmentLog> {
    match platform::open_in(dir, &log_segment(seq).0) {
        Ok(file) => Ok(SegmentLog::Written(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Packed::open(platform::open_in(dir, &packed_segment(seq))?).map(SegmentLog::Packed)
        }
        Err(e) => Err(e),
    }
}

/// A segment's log compressed ([`pack`]), read a frame at a time; the frame read last is
/// kept, as the next read is most often in it.
#[derive(Debug)]
pub struct Packed {
    file: File,
    frames: Vec<Frame>,
    /// The log's length, as written.
    len: u64,
    last: std::cell::RefCell<Option<(usize, Vec<u8>)>>,
}

/// A frame of a compressed log: where it is in the file, and where its part of the log
/// starts and how long it is.
#[derive(Debug, Clone, Copy)]
struct Frame {
    at: u64,
    packed: usize,
    start: u64,
    len: usize,
}

impl Packed {
    /// The compressed log `file`, its seek table read and checked against the file: its
    /// frames lie end to end before the table, each no larger than [`pack`] makes one.
    pub fn open(file: File) -> io::Result<Packed> {
        let damaged = |why: &str| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a compressed log segment: {why}"),
            )
        };
        let size = file.metadata()?.len();
        let mut footer = [0u8; FOOTER];
        let footer_at = size
            .checked_sub(FOOTER as u64)
            .ok_or_else(|| damaged("shorter than a seek table"))?;
        file.read_exact_at(&mut footer, footer_at)?;
        let [n0, n1, n2, n3, descriptor, m0, m1, m2, m3] = footer;
        if u32::from_le_bytes([m0, m1, m2, m3]) != SEEKABLE_MAGIC {
            return Err(damaged("no seek table"));
        }
        if descriptor & RESERVED != 0 {
            return Err(damaged("a seek table of a later version"));
        }
        let entry = if descriptor & CHECKSUMS != 0 {
            ENTRY + 4
        } else {
            ENTRY
        };
        let count =
            usize::try_from(u32::from_le_bytes([n0, n1, n2, n3])).map_err(|_| damaged("too many frames"))?;
        let table = count
            .checked_mul(entry)
            .and_then(|n| n.checked_add(FOOTER))
            .ok_or_else(|| damaged("too many frames"))?;
        let frames_end = size
            .checked_sub(8 + table as u64)
            .ok_or_else(|| damaged("a seek table longer than its file"))?;
        let mut head = vec![0u8; 8 + table - FOOTER];
        file.read_exact_at(&mut head, frames_end)?;
        let (magic, rest) = head
            .split_first_chunk::<4>()
            .ok_or_else(|| damaged("a seek table cut short"))?;
        let (frame_size, entries) = rest
            .split_first_chunk::<4>()
            .ok_or_else(|| damaged("a seek table cut short"))?;
        if u32::from_le_bytes(*magic) != SKIPPABLE_MAGIC
            || usize::try_from(u32::from_le_bytes(*frame_size)).ok() != Some(table)
        {
            return Err(damaged("a seek table not in its own frame"));
        }
        let mut frames = Vec::with_capacity(count);
        let (mut at, mut start) = (0u64, 0u64);
        for e in entries.chunks_exact(entry) {
            let field = |i: usize| {
                e.get(i..i + 4)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(u32::from_le_bytes)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| damaged("a seek table entry cut short"))
            };
            let (packed, len) = (field(0)?, field(4)?);
            if packed > FRAME_BOUND || len > FRAME {
                return Err(damaged("a frame larger than any written"));
            }
            frames.push(Frame {
                at,
                packed,
                start,
                len,
            });
            at += packed as u64;
            start += len as u64;
        }
        if at != frames_end {
            return Err(damaged("frames not where its seek table says"));
        }
        Ok(Packed {
            file,
            frames,
            len: start,
            last: std::cell::RefCell::new(None),
        })
    }

    /// Fills `buf` from the log at `at`, failing past its end.
    pub fn read_exact_at(&self, mut buf: &mut [u8], mut at: u64) -> io::Result<()> {
        while !buf.is_empty() {
            let i = self.frames.partition_point(|f| f.start + f.len as u64 <= at);
            let frame = self
                .frames
                .get(i)
                .copied()
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
            let mut last = self.last.borrow_mut();
            if last.as_ref().map(|(n, _)| *n) != Some(i) {
                *last = Some((i, self.unpack(&frame)?));
            }
            let bytes = last.as_ref().map(|(_, b)| b.as_slice()).unwrap_or_default();
            let from = usize::try_from(at - frame.start).unwrap_or(usize::MAX);
            let have = bytes.get(from..).unwrap_or_default();
            let n = have.len().min(buf.len());
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let (to, rest) = std::mem::take(&mut buf).split_at_mut(n);
            to.copy_from_slice(have.get(..n).unwrap_or_default());
            buf = rest;
            at += n as u64;
        }
        Ok(())
    }

    /// Frame `frame`, decompressed: its checksum checked, and no more read of it than its
    /// seek table says it holds.
    fn unpack(&self, frame: &Frame) -> io::Result<Vec<u8>> {
        let mut packed = vec![0u8; frame.packed];
        self.file.read_exact_at(&mut packed, frame.at)?;
        let mut out = Vec::with_capacity(frame.len);
        shards_image::store::Zstd::new(packed.as_slice())
            .take(frame.len as u64 + 1)
            .read_to_end(&mut out)?;
        if out.len() != frame.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a compressed log segment: a frame not as long as its seek table says",
            ));
        }
        Ok(out)
    }
}

/// A compression to do.
pub(super) enum Job {
    /// Rotated segment `seq` of container `id`'s log, in `dir`.
    Segment { id: String, dir: PathBuf, seq: u64 },
    /// Each rotated segment of container `id`'s log not yet compressed, in `dir`: what a
    /// daemon before this one left as it ended.
    Rotated { id: String, dir: PathBuf },
}

/// The compressions to do, and the thread that does them.
#[derive(Default)]
pub(super) struct Packing {
    queue: Mutex<VecDeque<Job>>,
    queued: Condvar,
    started: AtomicBool,
    /// The thread is to return once none is queued: a test's daemon's, as its scope ends.
    ended: AtomicBool,
}

impl Packing {
    /// Ends the thread once nothing is queued: a test's daemon's.
    #[cfg(test)]
    pub(super) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let _guard = lock(&self.queue);
        self.queued.notify_all();
    }
}

impl<D: Disk> Daemon<D> {
    /// Starts the packer's thread, unless it runs.
    pub(super) fn start_packer<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let p = &self.packing;
        if !p.started.swap(true, Ordering::SeqCst)
            && let Err(e) = std::thread::Builder::new()
                .name("packer".into())
                .spawn_scoped(threads, move || self.pack_all())
        {
            p.started.store(false, Ordering::SeqCst);
            log(format!(
                "the packer's thread: {e}; rotated log segments are compressed where they rotate"
            ));
        }
    }

    /// Has `job` done on the packer's thread, after those queued before it; here, where
    /// that thread does not run.
    pub(super) fn pack_soon(&self, job: Job) {
        let p = &self.packing;
        if !p.started.load(Ordering::SeqCst) {
            self.pack_job(job);
            return;
        }
        lock(&p.queue).push_back(job);
        p.queued.notify_one();
    }

    /// Has the rotated segments compressed that a daemon before this one left as they
    /// were written: those of each container whose log compresses them.
    pub(super) fn pack_left(&self) {
        let left: Vec<(String, PathBuf)> = {
            let registry = lock(&self.containers);
            registry
                .all()
                .filter(|c| c.log_compress)
                .map(|c| (c.id.clone(), registry.dir(&c.id)))
                .collect()
        };
        for (id, dir) in left {
            self.pack_soon(Job::Rotated { id, dir });
        }
    }

    /// Does the jobs queued, in turn, until [`Packing::end`].
    fn pack_all(&self) {
        let p = &self.packing;
        loop {
            let job = {
                let mut queue = lock(&p.queue);
                loop {
                    if let Some(job) = queue.pop_front() {
                        break job;
                    }
                    if p.ended.load(Ordering::SeqCst) {
                        return;
                    }
                    queue = p.queued.wait(queue).unwrap_or_else(PoisonError::into_inner);
                }
            };
            self.pack_job(job);
        }
    }

    fn pack_job(&self, job: Job) {
        let (id, dir, only) = match job {
            Job::Segment { id, dir, seq } => (id, dir, Some(seq)),
            Job::Rotated { id, dir } => (id, dir, None),
        };
        // One taken out of sight is being removed, with its files; one whose record is
        // still being written is not.
        if lock(&self.containers).made(&id).is_none() {
            return;
        }
        let packed = platform::open_dir(&dir).and_then(|dir| {
            let seqs = match only {
                Some(seq) => vec![seq],
                None => rotated(&dir)?,
            };
            for seq in seqs {
                pack(&dir, seq).map_err(|e| io::Error::new(e.kind(), format!("segment {seq}: {e}")))?;
            }
            Ok(())
        });
        if let Err(e) = packed {
            log(format!("container {id}: compressing its log: {e}"));
        }
    }
}

/// The segments of the log in `dir` that are rotated, every one but the newest, whose log
/// is there as written.
fn rotated(dir: &File) -> io::Result<Vec<u64>> {
    let names: Vec<String> = platform::names_in(dir)?
        .into_iter()
        .filter_map(|n| n.into_string().ok())
        .collect();
    let mut seqs: Vec<u64> = names.iter().filter_map(|n| super::logs::segment_of(n)).collect();
    seqs.sort_unstable();
    // The newest is its writer's still.
    seqs.pop();
    Ok(seqs
        .into_iter()
        .filter(|&seq| names.contains(&log_segment(seq).0))
        .collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// `len` bytes of lines that compress as logs do, varied enough that no frame is all
    /// repeats.
    fn lines(len: usize) -> Vec<u8> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut out = Vec::with_capacity(len);
        let mut n = 0u64;
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            n += 1;
            out.extend(format!("{n} request {} took {} us\n", x % 1000, x % 977).into_bytes());
        }
        out.truncate(len);
        out
    }

    fn dir_of(t: &shards_testdir::TempDir) -> File {
        platform::open_dir(t).unwrap()
    }

    /// A segment compressed reads back as written, from anywhere to anywhere, across its
    /// frames; its file is one `zstd -d` reads whole, as a decoder of every frame that
    /// skips skippable ones reads it (RFC 8878 §3.1.2); and the log as written is gone.
    #[test]
    fn a_packed_segment_reads_back_as_written() {
        let t = shards_testdir::TempDir::new("packed-reads").unwrap();
        let dir = dir_of(&t);
        let written = lines(3 * FRAME + 12_345);
        std::fs::write(t.join("log.1"), &written).unwrap();
        std::fs::write(t.join("log.1.idx"), []).unwrap();
        assert!(pack(&dir, 1).unwrap());
        assert!(!t.join("log.1").exists(), "the log as written stayed");
        let SegmentLog::Packed(packed) = open_log(&dir, 1).unwrap() else {
            panic!("not read compressed");
        };
        assert_eq!(packed.len, written.len() as u64);
        assert_eq!(packed.frames.len(), 4);
        let mut x = 7u64;
        for _ in 0..200 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let at = (x >> 33) as usize % written.len();
            let len = ((x >> 13) as usize % (2 * FRAME)).min(written.len() - at);
            let mut buf = vec![0u8; len];
            packed.read_exact_at(&mut buf, at as u64).unwrap();
            assert_eq!(buf, written[at..at + len], "at {at}, {len} bytes");
        }
        let mut past = [0u8; 2];
        let end = written.len() as u64 - 1;
        assert_eq!(
            packed.read_exact_at(&mut past, end).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let file = std::fs::read(t.join("log.1.zst")).unwrap();
        let mut whole = Vec::new();
        shards_image::store::Zstd::new(file.as_slice())
            .read_to_end(&mut whole)
            .unwrap();
        assert_eq!(whole, written);
        // Packed again, it is there already: nothing to do.
        assert!(!pack(&dir, 1).unwrap());
    }

    /// A compressed segment that is not as written is refused, never read as output: a
    /// seek table damaged, of a later version, that claims frames larger than any written
    /// or where none are; and a frame whose bytes changed fails its checksum.
    #[test]
    fn a_damaged_packed_segment_is_refused() {
        let t = shards_testdir::TempDir::new("packed-damage").unwrap();
        let dir = dir_of(&t);
        let written = lines(FRAME + 100);
        std::fs::write(t.join("log"), &written).unwrap();
        std::fs::write(t.join("log.idx"), []).unwrap();
        assert!(pack(&dir, 0).unwrap());
        let good = std::fs::read(t.join("log.zst")).unwrap();
        let n = good.len();
        // The table: magic and size, two entries, then the footer.
        let table = n - (8 + 2 * ENTRY + FOOTER);
        type Damage = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Damage)> = vec![
            ("no seek table", Box::new(move |f| f[n - 1] ^= 1)),
            ("a later version", Box::new(move |f| f[n - 5] |= 0x04)),
            ("not in its own frame", Box::new(move |f| f[table] ^= 1)),
            (
                "larger than any written",
                Box::new(move |f| f[table + 8 + 4 + 2] = 0x20),
            ),
            ("not where", Box::new(move |f| f[table + 8] ^= 1)),
            ("cut short", Box::new(|f| f.truncate(4))),
        ];
        for (said, damage) in cases {
            let mut file = good.clone();
            damage(&mut file);
            std::fs::write(t.join("log.zst"), &file).unwrap();
            let e = open_log(&dir, 0).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{said}: {e}");
        }
        // A byte of the first frame's content changed: its checksum fails on reading.
        let mut file = good.clone();
        file[40] ^= 0x55;
        std::fs::write(t.join("log.zst"), &file).unwrap();
        let log = open_log(&dir, 0).unwrap();
        let mut buf = vec![0u8; 16];
        assert!(log.read_exact_at(&mut buf, 0).is_err());
    }

    /// A segment removed as it is compressed, the oldest past those its log keeps, leaves
    /// nothing of itself; one gone already has nothing to compress.
    #[test]
    fn a_segment_removed_as_it_is_packed_leaves_nothing() {
        let t = shards_testdir::TempDir::new("packed-removed").unwrap();
        let dir = dir_of(&t);
        std::fs::write(t.join("log.2"), lines(1000)).unwrap();
        // Its index gone first, as new_segment removes a segment.
        assert!(pack(&dir, 2).unwrap());
        assert!(!t.join("log.2").exists() && !t.join("log.2.zst").exists());
        assert!(!pack(&dir, 3).unwrap());
    }

    /// The segments compressed as a daemon starts are those before the newest still as
    /// written: not the newest, which its writer may write again, nor one compressed.
    #[test]
    fn the_rotated_segments_are_those_before_the_newest() {
        let t = shards_testdir::TempDir::new("packed-rotated").unwrap();
        for name in [
            "log",
            "log.idx",
            "log.1",
            "log.1.idx",
            "log.2.zst",
            "log.2.idx",
            "log.3",
            "log.3.idx",
        ] {
            std::fs::write(t.join(name), b"x").unwrap();
        }
        assert_eq!(rotated(&dir_of(&t)).unwrap(), [0, 1]);
        let t = shards_testdir::TempDir::new("packed-one").unwrap();
        std::fs::write(t.join("log"), b"x").unwrap();
        std::fs::write(t.join("log.idx"), b"x").unwrap();
        assert!(rotated(&dir_of(&t)).unwrap().is_empty());
    }

    /// Making a segment removes the oldest past those the log keeps, compressed or not.
    #[test]
    fn retention_removes_packed_segments_too() {
        let t = shards_testdir::TempDir::new("packed-retention").unwrap();
        let dir = dir_of(&t);
        for name in ["log.zst", "log.idx", "log.1", "log.1.idx"] {
            std::fs::write(t.join(name), b"x").unwrap();
        }
        crate::segments::new_segment(&dir, 2, 2).unwrap();
        let mut names: Vec<String> = platform::names_in(&dir)
            .unwrap()
            .into_iter()
            .filter_map(|n| n.into_string().ok())
            .collect();
        names.sort();
        assert_eq!(names, ["log.1", "log.1.idx", "log.2", "log.2.idx"]);
    }

    /// The packer's own: what [`pack_with`] makes, durable or not, is the same file.
    #[test]
    fn packing_without_flushes_makes_the_same_file() {
        let t = shards_testdir::TempDir::new("packed-same").unwrap();
        let dir = dir_of(&t);
        let written = lines(FRAME / 2);
        for seq in [0, 1] {
            let (log, index) = log_segment(seq);
            std::fs::write(t.join(log), &written).unwrap();
            std::fs::write(t.join(index), []).unwrap();
        }
        assert!(pack_with(&dir, 0, true).unwrap());
        assert!(pack_with(&dir, 1, false).unwrap());
        assert_eq!(
            std::fs::read(t.join("log.zst")).unwrap(),
            std::fs::read(t.join("log.1.zst")).unwrap()
        );
    }
}
