//! A move's stream packed for the link it crosses (D176 parts 6 and 7): cut into frames of
//! [`FRAME`] bytes, each sent as zstd where that is smaller (shards-zstd, klauspost's
//! encoder, which layers are compressed with; ruzstd reads it, through
//! `shards_image::store::Zstd`), else as it is, at the level the link leaves room for.
//!
//! Compression overlaps the link and its effort follows what the link and the processor leave
//! room for, as AdOC's does (Jeannot, Knutsson and Björkman, "Adaptive Online Data
//! Compression", HPDC 2002, DOI 10.1109/HPDC.2002.1029938: "the slower the network, the more
//! we use efficient and slow compression algorithms"; compressing whole before sending loses
//! on a fast link). Here the level is set frame by frame by who waits on whom: the packer
//! compresses a frame while the link carries the ones before it, through a queue of
//! [`AHEAD`] frames. One that finds the queue full is ahead of the link, and waits for it:
//! where that wait is longer than the next level up took more than this one to compress a
//! frame, as that level last did (or it has not been tried), the next frame goes a level up,
//! the link's spare time spent on a better ratio. One whose reader waited for it, the
//! packer's compressing longer than its waiting for what it packs, is behind: the next goes a
//! level down, to none, as on a link faster than the compressor, whose frames then go as
//! they are, and stay so, as no wait of the packer's there covers what compressing costs.
//! Nothing is measured but the stream's own waits and its compressing, and nothing chosen but
//! the levels klauspost's encoder has.
//!
//! The stream: [`MAGIC`], then frames, each its kind ([`RAW`] or [`ZSTD`]), its length as it
//! was and as sent, little-endian `u32`s, and its bytes; it ends where its carrier does, at a
//! frame's end, the move's own stream holding its end within it. The machine moved to says it
//! takes it ([`CODEC`] among what its `ready` answers it unpacks), and is sent it only then: a
//! daemon of an earlier build is sent the stream as it is.

use std::io::{self, Read};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// What a daemon that reads a packed stream says it unpacks (daemon/moves.rs `Needs`), and
/// what its import is told the stream is (`import --packed zstd`).
pub(crate) const CODEC: &str = "zstd";
/// The stream's first bytes.
const MAGIC: [u8; 8] = *b"SHRDPACK";
/// A frame's kinds.
const RAW: u8 = 0;
const ZSTD: u8 = 1;
/// The most a frame holds, as it was and as sent: zstd's frames of 1 MiB of real logs keep
/// 0.07 to 0.10 points more of their size than the whole compressed at once (PM M231: 9.24%
/// against 9.17% at level 1, 7.63% against 7.53% at 11), and each is a reader's to hold
/// whole.
const FRAME: usize = 1 << 20;
/// How many frames wait for the link before the packer is ahead of it.
const AHEAD: usize = 2;
/// The levels a frame is packed at, the lowest first: none, then klauspost's four
/// (`EncoderLevelFromZstd`: SpeedFastest, SpeedDefault, SpeedBetterCompression,
/// SpeedBestCompression).
const LEVELS: [Option<i32>; 5] = [None, Some(1), Some(3), Some(7), Some(11)];
/// Where a stream begins: zstd's fastest, as the link is not yet known.
const FIRST: usize = 1;

/// The level frames go at ([`LEVELS`], by index), and what each level last took to compress
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pace {
    level: usize,
    costs: [Option<Duration>; LEVELS.len()],
}

impl Pace {
    fn new() -> Pace {
        Pace {
            level: FIRST,
            costs: [None; LEVELS.len()],
        }
    }

    fn level(&self) -> Option<i32> {
        LEVELS.get(self.level).copied().flatten()
    }

    /// After a frame at its level, which took `compressing` to compress and `reading` to
    /// read: `waited`, how long the packer then waited for the link to take it, where it
    /// found the queue full; `starved`, whether the reader waited for it. Down where the
    /// reader waited on compressing; up where the packer's wait covers what the next level
    /// takes more than this one, as it last did, or it has not been tried; else as it was.
    fn after(&mut self, compressing: Duration, reading: Duration, waited: Option<Duration>, starved: bool) {
        if let Some(cost) = self.costs.get_mut(self.level) {
            *cost = Some(compressing);
        }
        if starved && compressing > reading {
            self.level = self.level.saturating_sub(1);
            return;
        }
        let up = self.level.saturating_add(1);
        if let (Some(waited), Some(cost)) = (waited, self.costs.get(up)) {
            let more = cost.map_or(Duration::ZERO, |c| c.saturating_sub(compressing));
            if waited > more {
                self.level = up;
            }
        }
    }
}

/// Whether the machine moved to, by what its `ready` says it unpacks, is sent the stream
/// packed: by its [`CODEC`].
pub(crate) fn taken(unpacks: &[String]) -> bool {
    unpacks.iter().any(|c| c == CODEC)
}

/// What a packed stream carried so far, as its packer counts it: its bytes as they were and as
/// sent, and its frames, those sent as zstd among them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub raw: u64,
    pub wire: u64,
    pub frames: u64,
    pub packed: u64,
}

/// `raw` packed at `level` ([`LEVELS`]): as zstd where that is smaller, else as it is.
fn frame(level: Option<i32>, raw: &[u8]) -> io::Result<Vec<u8>> {
    let packed = match level {
        Some(level) => Some(shards_zstd::compress(Some(level), raw)?).filter(|z| z.len() < raw.len()),
        None => None,
    };
    let (kind, body) = match &packed {
        Some(z) => (ZSTD, z.as_slice()),
        None => (RAW, raw),
    };
    let long = |n: usize| u32::try_from(n).map_err(|_| io::Error::other("a frame past 4 GiB"));
    let mut out = Vec::with_capacity(body.len() + 9);
    out.push(kind);
    out.extend_from_slice(&long(raw.len())?.to_le_bytes());
    out.extend_from_slice(&long(body.len())?.to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Reads `buf` full from `r`, or to its end: how much it read.
fn fill(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while let Some(rest) = buf.get_mut(n..).filter(|rest| !rest.is_empty()) {
        match r.read(rest) {
            Ok(0) => break,
            Ok(got) => n += got,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// `input` packed, read from the [`Packing`] returned as a thread of its own packs it, and
/// where what it carried is told, after each frame ([`Tally`]). An input that fails ends the
/// stream there, which its reader then finds cut short.
pub(crate) fn pack(mut input: impl Read + Send + 'static) -> io::Result<(Packing, mpsc::Receiver<Tally>)> {
    let (frames, from) = mpsc::sync_channel::<Vec<u8>>(AHEAD);
    let (starved, waited) = mpsc::channel::<()>();
    let (told, tallies) = mpsc::channel();
    std::thread::Builder::new()
        .name("move-pack".into())
        .spawn(move || {
            if frames.send(MAGIC.to_vec()).is_err() {
                return;
            }
            let mut tally = Tally::default();
            let mut pace = Pace::new();
            let mut raw = vec![0u8; FRAME];
            loop {
                let t = Instant::now();
                let n = match fill(&mut input, &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let reading = t.elapsed();
                let t = Instant::now();
                let Ok(out) = frame(pace.level(), raw.get(..n).unwrap_or_default()) else {
                    return;
                };
                let compressing = t.elapsed();
                tally.raw = tally.raw.saturating_add(n as u64);
                tally.wire = tally.wire.saturating_add(out.len() as u64);
                tally.frames = tally.frames.saturating_add(1);
                if out.first() == Some(&ZSTD) {
                    tally.packed = tally.packed.saturating_add(1);
                }
                let ahead = match frames.try_send(out) {
                    Ok(()) => None,
                    Err(mpsc::TrySendError::Full(out)) => {
                        let t = Instant::now();
                        if frames.send(out).is_err() {
                            return;
                        }
                        Some(t.elapsed())
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                };
                let _ = told.send(tally);
                let starved = waited.try_iter().count() > 0;
                pace.after(compressing, reading, ahead, starved);
            }
        })?;
    Ok((
        Packing {
            frames: from,
            starved,
            current: Vec::new(),
            at: 0,
            taken: 0,
        },
        tallies,
    ))
}

/// A packed stream's reading end ([`pack`]).
#[derive(Debug)]
pub(crate) struct Packing {
    frames: mpsc::Receiver<Vec<u8>>,
    starved: mpsc::Sender<()>,
    current: Vec<u8>,
    at: usize,
    /// How many frames it has taken, the magic the first: a wait for the first frame of the
    /// stream is no packer's slowness, as there is nothing before it to read.
    taken: u64,
}

impl Read for Packing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at >= self.current.len() {
            let next = match self.frames.try_recv() {
                Ok(next) => Some(next),
                Err(mpsc::TryRecvError::Empty) => {
                    if self.taken >= 2 {
                        let _ = self.starved.send(());
                    }
                    self.frames.recv().ok()
                }
                Err(mpsc::TryRecvError::Disconnected) => None,
            };
            let Some(next) = next else {
                return Ok(0);
            };
            self.taken = self.taken.saturating_add(1);
            self.current = next;
            self.at = 0;
        }
        let rest = self.current.get(self.at..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        if let (Some(to), Some(from)) = (buf.get_mut(..n), rest.get(..n)) {
            to.copy_from_slice(from);
        }
        self.at += n;
        Ok(n)
    }
}

/// A packed stream ([`pack`]) read from `src` as it was: each frame checked whole, as long as
/// it says and no longer than [`FRAME`], before any of it is given.
#[derive(Debug)]
pub(crate) struct Unpacked<R> {
    src: R,
    begun: bool,
    current: Vec<u8>,
    at: usize,
}

impl<R: Read> Unpacked<R> {
    pub(crate) fn new(src: R) -> Unpacked<R> {
        Unpacked {
            src,
            begun: false,
            current: Vec::new(),
            at: 0,
        }
    }

    /// The next frame, as it was; none at the stream's end, which comes between frames.
    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        let bad = |what: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the move's packed stream: {what}"),
            )
        };
        if !self.begun {
            let mut magic = [0u8; 8];
            self.src.read_exact(&mut magic)?;
            if magic != MAGIC {
                return Err(bad("not one".into()));
            }
            self.begun = true;
        }
        let mut kind = [0u8; 1];
        loop {
            match self.src.read(&mut kind) {
                Ok(0) => return Ok(None),
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let mut lens = [0u8; 8];
        self.src.read_exact(&mut lens)?;
        let (was, sent) = lens.split_at(4);
        let len = |b: &[u8]| -> io::Result<usize> {
            let n = <[u8; 4]>::try_from(b)
                .map(u32::from_le_bytes)
                .map_err(|_| bad("a length".into()))?;
            usize::try_from(n)
                .ok()
                .filter(|n| (1..=FRAME).contains(n))
                .ok_or_else(|| bad(format!("a frame of {n} bytes")))
        };
        let (was, sent) = (len(was)?, len(sent)?);
        let mut body = vec![0u8; sent];
        self.src.read_exact(&mut body)?;
        match kind[0] {
            RAW if sent == was => Ok(Some(body)),
            RAW => Err(bad(format!("a frame of {was} bytes sent as {sent}"))),
            ZSTD => {
                let mut out = Vec::with_capacity(was);
                let limit = u64::try_from(was).map_or(u64::MAX, |w| w.saturating_add(1));
                shards_image::store::Zstd::new(body.as_slice())
                    .take(limit)
                    .read_to_end(&mut out)?;
                if out.len() == was {
                    Ok(Some(out))
                } else {
                    Err(bad(format!("a frame of {was} bytes unpacked to {}", out.len())))
                }
            }
            other => Err(bad(format!("a frame of kind {other}"))),
        }
    }
}

impl<R: Read> Read for Unpacked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at >= self.current.len() {
            match self.next()? {
                Some(next) => {
                    self.current = next;
                    self.at = 0;
                }
                None => return Ok(0),
            }
        }
        let rest = self.current.get(self.at..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        if let (Some(to), Some(from)) = (buf.get_mut(..n), rest.get(..n)) {
            to.copy_from_slice(from);
        }
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A stream of what a move sends, packed and read back as it was: memory's pages of
    /// zeros and of text, which shrink, and random bytes, which go as they are; a stream cut
    /// mid-frame, or a frame past its bound, refused.
    #[test]
    fn a_packed_stream_reads_back_as_it_was() {
        let mut stream = Vec::new();
        stream.extend(std::iter::repeat_n(0u8, 3 * FRAME + 123));
        for i in 0..200_000u32 {
            stream.extend_from_slice(format!("page {i} of a guest's memory\n").as_bytes());
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..FRAME {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            stream.push(seed as u8);
        }
        // Read as fast as memory goes, faster than any compressor: its first frame, zeros,
        // goes as zstd, at the first level, and the random one as it is, whatever the levels
        // between them came to.
        let (mut packing, tallies) = pack(io::Cursor::new(stream.clone())).unwrap();
        let mut wire = Vec::new();
        packing.read_to_end(&mut wire).unwrap();
        let tally = tallies.iter().last().unwrap();
        assert_eq!(tally.raw, stream.len() as u64);
        assert_eq!(tally.wire + MAGIC.len() as u64, wire.len() as u64);
        assert!(tally.packed > 0 && tally.packed < tally.frames, "{tally:?}");
        assert_eq!(wire[MAGIC.len()], ZSTD);
        let mut back = Vec::new();
        Unpacked::new(wire.as_slice()).read_to_end(&mut back).unwrap();
        assert_eq!(back, stream);

        // Each level shrinks what shrinks, and none grows what does not.
        let (zeros, text, random) = (
            &stream[..FRAME],
            &stream[3 * FRAME + 123..4 * FRAME + 123],
            &stream[stream.len() - FRAME..],
        );
        for level in LEVELS {
            for (raw, shrinks) in [(zeros, true), (text, true), (random, false)] {
                let f = frame(level, raw).unwrap();
                let packed = level.is_some() && shrinks;
                assert_eq!(f[0], if packed { ZSTD } else { RAW }, "{level:?}");
                if packed {
                    assert!(f.len() < raw.len() / 4, "{level:?}: {} of {}", f.len(), raw.len());
                }
                let mut one = MAGIC.to_vec();
                one.extend_from_slice(&f);
                let mut back = Vec::new();
                Unpacked::new(one.as_slice()).read_to_end(&mut back).unwrap();
                assert_eq!(back, raw);
            }
        }

        // Cut short within a frame: refused, never taken for its end.
        let cut = &wire[..wire.len() - 10];
        assert!(Unpacked::new(cut).read_to_end(&mut Vec::new()).is_err());
        // A frame saying more than a frame holds.
        let mut big = MAGIC.to_vec();
        big.push(RAW);
        big.extend_from_slice(&(FRAME as u32 + 1).to_le_bytes());
        big.extend_from_slice(&(FRAME as u32 + 1).to_le_bytes());
        assert!(
            Unpacked::new(big.as_slice())
                .read_to_end(&mut Vec::new())
                .is_err()
        );
        // Not a packed stream at all: a move's own, sent to a reader told it was packed.
        assert!(
            Unpacked::new(&b"SHRDMOVE\x02\x00\x00\x00"[..])
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }

    /// The level follows who waits. On a slow link (a frame of 1 MiB at 0.55 MB/s: the
    /// packer waits 1.9 s for each) it climbs a level a frame to the best; on one faster than
    /// the compressor the reader waits, and it falls to none and stays there, as the packer's
    /// wait for that link (0.8 ms a frame at 10 Gb/s) covers no level's cost, once that is
    /// known; a reader's wait on the packer's own input moves nothing, nor a frame neither
    /// waited on.
    #[test]
    fn the_level_follows_who_waits() {
        let ms = Duration::from_millis;
        let read = Duration::from_micros(50);
        let mut slow = Pace::new();
        for _ in 0..10 {
            let took = ms(2 * slow.level as u64 + 1);
            slow.after(took, read, Some(ms(1900)), false);
        }
        assert_eq!(slow.level(), Some(11));

        let mut fast = Pace::new();
        // Level 1 starves the reader: down to none, its cost of 2 ms known.
        fast.after(ms(2), read, None, true);
        assert_eq!(fast.level(), None);
        // At none the packer is ahead, by 0.8 ms a frame, short of the 2 ms level 1 costs.
        for _ in 0..10 {
            fast.after(
                Duration::from_micros(30),
                read,
                Some(Duration::from_micros(800)),
                false,
            );
        }
        assert_eq!(fast.level(), None);
        // A level not yet tried is tried once.
        let mut fresh = Pace::new();
        fresh.level = 0;
        fresh.after(
            Duration::from_micros(30),
            read,
            Some(Duration::from_micros(800)),
            false,
        );
        assert_eq!(fresh.level(), Some(1));

        // The reader waited on the packer's input, not its compressing: as it was.
        let mut input = Pace::new();
        input.level = 2;
        input.after(ms(1), ms(40), None, true);
        assert_eq!(input.level, 2);
        // Neither waited: as it was.
        input.after(ms(1), read, None, false);
        assert_eq!(input.level, 2);
        // Ahead and waited on both (a link that varies): the wait on it counts first.
        input.after(ms(5), read, Some(ms(100)), true);
        assert_eq!(input.level, 1);
    }

    /// A machine moved to is sent the stream packed only where its `ready` says it unpacks
    /// zstd: a daemon of an earlier build, whose answer names nothing it unpacks, is sent it
    /// as it is.
    #[test]
    fn a_stream_is_packed_only_for_a_daemon_that_unpacks_it() {
        assert!(taken(&["zstd".to_string()]));
        assert!(taken(&["lz4".to_string(), "zstd".to_string()]));
        assert!(!taken(&[]));
        assert!(!taken(&["lz4".to_string()]));
    }
}
