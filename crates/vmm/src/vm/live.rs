//! A running microVM moved live (D176 part 7): its memory copied to the other machine as
//! its guest runs on, round after round, each round the pages its guest and its devices
//! wrote since the last ([`Pages`]), until what is left is little; then the machine saved
//! as a save does, held, and the pages written since that last round sent as the stream's
//! last round ([`Writer::going_on`]), which the other machine lays over the others page by
//! page, as it lays each round over the one before.
//!
//! The rounds' stream ([`Writer`]): a magic, then records, each a kind, an offset and a run
//! of bytes, into what a snapshot keeps: its memory file's RAM by its offset there, the
//! memory plugged by its guest-physical address (its place in the memory file is known only
//! once the last plugged run is), its writable layer's file system and its run's swap by
//! their offsets; and an end. What a snapshot keeps besides (its state, the heads its guest
//! wrote page structures into) is the save's, whole; its memory, layer and swap files hold
//! none of their pages, which the stream holds.

use std::io::{self, Write};

/// The unit pages are noted and sent in: 4 KiB, whatever the hypervisor's own page.
pub const PAGE: u64 = 4096;

/// The stream's first bytes.
pub const MAGIC: [u8; 8] = *b"SHRDLIVE";
/// Its records' kinds.
pub const END: u8 = 0;
pub const RAM: u8 = 1;
pub const PLUGGED: u8 = 2;
pub const LAYER: u8 = 3;
pub const SWAP: u8 = 4;
/// A round's end: its number, and the pages and bytes it sent.
pub const ROUND: u8 = 5;

/// Why a live move's pre-copy stopped and its last save began (D176 part 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Its last round sent no more than the bytes a last round may: what is left is little
    /// (Clark et al., NSDI 2005, §5.3: "less than 256KB remains"; Xen's xc_domain_save, fewer
    /// than 50 pages dirtied).
    Little,
    /// Its last round sent no fewer bytes than the round before: its guest writes as fast as
    /// the link carries what it wrote. A round sends what was written as the round before it
    /// went, so its bytes over that round's time are the guest's rate of writing, and the
    /// round before's bytes over the same time the link's rate: rounds that no longer shrink
    /// say the first is the greater, and another round would leave as much to the last save
    /// as this one does, at the cost of a round (Clark et al. §4.1, "the hottest pages will
    /// be dirtied at least as fast as we can transfer them, and hence must be transferred in
    /// the final stop-and-copy phase"; §6.5, a guest writing faster than the network carries
    /// suspended after the rounds that did not outpace it).
    Unshrinking,
    /// It has sent three times the bytes its first round sent, all its memory held: Xen's
    /// bound, "never send more than 3x" its memory (xc_domain_save.c, `DEF_MAX_FACTOR`).
    Thrice,
    /// It has run its most rounds: Xen's 30 (`DEF_MAX_ITERS` 29, "limit us to 30 times round
    /// loop").
    Rounds,
}

impl Stop {
    /// Whether, and why, a pre-copy stops after rounds that sent `sent` bytes each, in order,
    /// the first all its memory: at its `most` rounds (at least one), or once one sends no
    /// more than `enough`; and once rounds no longer shrink, or what they sent comes to three
    /// times the first's. None before any round: the first always runs. Rounds that each
    /// shrink end too, as each is fewer bytes, of pages, than the one before.
    pub fn after(sent: &[u64], most: u32, enough: u64) -> Option<Stop> {
        let &last = sent.last()?;
        if last <= enough {
            return Some(Stop::Little);
        }
        if let [.., before, _] = sent
            && last >= *before
        {
            return Some(Stop::Unshrinking);
        }
        let first = sent.first().copied().unwrap_or(0);
        let total = sent.iter().fold(0u64, |a, b| a.saturating_add(*b));
        if sent.len() > 1 && total > first.saturating_mul(3) {
            return Some(Stop::Thrice);
        }
        if sent.len() >= most.max(1) as usize {
            return Some(Stop::Rounds);
        }
        None
    }

    /// As a move's log says it.
    pub fn said(self) -> &'static str {
        match self {
            Stop::Little => "little left",
            Stop::Unshrinking => "its rounds no longer shrink",
            Stop::Thrice => "three times its memory sent",
            Stop::Rounds => "its most rounds",
        }
    }
}

/// Pages of guest memory, by guest-physical address, each [`PAGE`] long: sorted, each once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pages(Vec<u64>);

impl Pages {
    /// `pages`, each `unit` bytes long at its address (a hypervisor's page, 4 KiB or 16
    /// KiB), as the [`PAGE`]s they hold.
    pub fn of(pages: impl IntoIterator<Item = u64>, unit: u64) -> Pages {
        let per = (unit / PAGE).max(1);
        let mut all: Vec<u64> = pages
            .into_iter()
            .flat_map(|p| {
                let first = p & !(PAGE - 1);
                (0..per).map(move |i| first.saturating_add(i * PAGE))
            })
            .collect();
        all.sort_unstable();
        all.dedup();
        Pages(all)
    }

    /// These and `other`'s.
    pub fn merge(&mut self, other: Pages) {
        self.0.extend(other.0);
        self.0.sort_unstable();
        self.0.dedup();
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().copied()
    }
}

/// Writes a live move's stream (the module's words) into `out`.
#[derive(Debug)]
pub struct Writer<W: Write> {
    out: W,
    /// The pages and bytes of records written since the round began.
    pages: u64,
    bytes: u64,
}

impl<W: Write> Writer<W> {
    pub fn new(mut out: W) -> io::Result<Writer<W>> {
        out.write_all(&MAGIC)?;
        Ok(Writer {
            out,
            pages: 0,
            bytes: 0,
        })
    }

    /// The stream [`new`](Self::new) began in `out`, written on after its rounds, which
    /// wrote no end: a live move's last save sends its last round so, the stream its spawner
    /// hands it again (D176 part 7).
    pub fn going_on(out: W) -> Writer<W> {
        Writer {
            out,
            pages: 0,
            bytes: 0,
        }
    }

    /// A record of `kind`: `bytes` at `offset`.
    pub fn record(&mut self, kind: u8, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let len = u32::try_from(bytes.len()).map_err(|_| io::Error::other("a record past 4 GiB"))?;
        self.out.write_all(&[kind])?;
        self.out.write_all(&offset.to_le_bytes())?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(bytes)?;
        self.pages = self.pages.saturating_add(u64::from(len).div_ceil(PAGE));
        self.bytes = self.bytes.saturating_add(u64::from(len));
        Ok(())
    }

    /// Ends round `n`: what it sent, said in the stream and returned, and counted afresh.
    pub fn round(&mut self, n: u32) -> io::Result<(u64, u64)> {
        let said = (self.pages, self.bytes);
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&said.0.to_le_bytes());
        body.extend_from_slice(&said.1.to_le_bytes());
        self.out.write_all(&[ROUND])?;
        self.out.write_all(&u64::from(n).to_le_bytes())?;
        self.out.write_all(&(body.len() as u32).to_le_bytes())?;
        self.out.write_all(&body)?;
        self.out.flush()?;
        self.pages = 0;
        self.bytes = 0;
        Ok(said)
    }

    /// The stream's end.
    pub fn finish(mut self) -> io::Result<()> {
        self.out.write_all(&[END])?;
        self.out.write_all(&0u64.to_le_bytes())?;
        self.out.write_all(&0u32.to_le_bytes())?;
        self.out.flush()
    }
}

/// A record [`read`] gives its taker: its kind, offset and bytes.
#[derive(Debug)]
pub struct Record<'a> {
    pub kind: u8,
    pub offset: u64,
    pub bytes: &'a [u8],
}

/// The most bytes a record holds, the run of pages a writer sends at once: a block of a
/// snapshot's memory file ([`DATA_BLOCK`](crate::memory::DATA_BLOCK), Linux's pageblock of
/// 4 KiB pages, D151), the unit the file says it holds data in and a restore maps.
pub const MAX_RECORD: usize = crate::memory::DATA_BLOCK;

/// Reads a live move's stream from `r`, each record given to `take` in order, up to its
/// end. A stream cut short, or a record past [`MAX_RECORD`], is refused.
pub fn read(
    r: &mut dyn io::Read,
    take: &mut dyn FnMut(Record<'_>) -> Result<(), String>,
) -> Result<(), String> {
    let io = |e: io::Error| format!("the live move's stream: {e}");
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(io)?;
    if magic != MAGIC {
        return Err("the live move's stream is not one".into());
    }
    let mut buf = vec![0u8; MAX_RECORD];
    loop {
        let mut kind = [0u8; 1];
        let mut offset = [0u8; 8];
        let mut len = [0u8; 4];
        r.read_exact(&mut kind).map_err(io)?;
        r.read_exact(&mut offset).map_err(io)?;
        r.read_exact(&mut len).map_err(io)?;
        let [kind] = kind;
        let offset = u64::from_le_bytes(offset);
        let len = u32::from_le_bytes(len) as usize;
        if kind == END {
            return Ok(());
        }
        let bytes = buf
            .get_mut(..len)
            .ok_or_else(|| format!("a record of {len} bytes, more than a live move sends at once"))?;
        r.read_exact(bytes).map_err(io)?;
        take(Record { kind, offset, bytes })?;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// Pre-copy goes on while its rounds shrink, and stops at the first that leaves little,
    /// or sends no fewer bytes than the one before, or brings what was sent to three times the
    /// first round, or is its most; never before its first round. A guest writing faster than
    /// the link carries (the two-host harness's 1 MiB rewritten every 50 ms, over a link of
    /// 0.55 MB/s) stops after its second round, where 30 rounds of it ran past 30 minutes.
    #[test]
    fn precopy_stops_once_its_rounds_no_longer_shrink() {
        let (most, enough) = (30, 8 * MIB);
        assert_eq!(Stop::after(&[], most, enough), None);
        // A guest that settles: on until little is left.
        assert_eq!(Stop::after(&[74 * MIB], most, enough), None);
        assert_eq!(Stop::after(&[74 * MIB, 20 * MIB], most, enough), None);
        assert_eq!(
            Stop::after(&[74 * MIB, 20 * MIB, 6 * MIB], most, enough),
            Some(Stop::Little)
        );
        // One writing as fast as the link carries: the second round as long as the first.
        assert_eq!(Stop::after(&[60 * MIB, 40 * MIB], most, enough), None);
        assert_eq!(
            Stop::after(&[60 * MIB, 40 * MIB, 40 * MIB], most, enough),
            Some(Stop::Unshrinking)
        );
        assert_eq!(
            Stop::after(&[60 * MIB, 40 * MIB, 41 * MIB], most, enough),
            Some(Stop::Unshrinking)
        );
        // Shrinking slowly: on until three times the first round is sent.
        let mut slow = vec![30 * MIB];
        let mut round = 29 * MIB;
        while Stop::after(&slow, most, enough).is_none() {
            slow.push(round);
            round -= MIB / 4;
        }
        assert_eq!(Stop::after(&slow, most, enough), Some(Stop::Thrice));
        assert_eq!(slow.len(), 4);
        // Each round a page fewer, never three times the first (a first round of 1 GiB):
        // its most rounds.
        let mut pages = vec![1024 * MIB, 9 * MIB];
        while Stop::after(&pages, most, enough).is_none() {
            pages.push(pages[pages.len() - 1] - PAGE);
        }
        assert_eq!(
            (Stop::after(&pages, most, enough), pages.len()),
            (Some(Stop::Rounds), 30)
        );
        // At most one round, which stops it whatever it sent.
        assert_eq!(Stop::after(&[74 * MIB], 1, enough), Some(Stop::Rounds));
        assert_eq!(Stop::after(&[74 * MIB], 0, enough), Some(Stop::Rounds));
    }

    /// Pages noted in a hypervisor's 16 KiB are each of their four 4 KiB, once, in order.
    #[test]
    fn pages_are_4k_each_once() {
        let mut p = Pages::of([0x10_0000 + 0x4000, 0x10_0000], 0x4000);
        assert_eq!(p.len(), 8);
        p.merge(Pages::of([0x10_0000 + 0x1000, 0x20_0000], PAGE));
        assert_eq!(p.len(), 9);
        assert!(p.iter().all(|a| a % PAGE == 0));
        let all: Vec<u64> = p.iter().collect();
        assert!(all.windows(2).all(|w| w[0] < w[1]), "{all:x?}");
        assert_eq!((all[0], all[8]), (0x10_0000, 0x20_0000));
    }

    /// A stream of rounds reads back record by record, to its end; cut short, it is refused.
    #[test]
    fn a_stream_reads_back_whole() {
        let mut bytes = Vec::new();
        let mut w = Writer::new(&mut bytes).unwrap();
        w.record(RAM, 0x1000, &[1; 4096]).unwrap();
        w.record(PLUGGED, 0x1_0000_0000, &[2; 8192]).unwrap();
        assert_eq!(w.round(0).unwrap(), (3, 12288));
        w.record(LAYER, 0, &[3; 4096]).unwrap();
        w.finish().unwrap();
        let mut seen = Vec::new();
        read(&mut bytes.as_slice(), &mut |r| {
            seen.push((r.kind, r.offset, r.bytes.len(), r.bytes.first().copied()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0], (RAM, 0x1000, 4096, Some(1)));
        assert_eq!(seen[1], (PLUGGED, 0x1_0000_0000, 8192, Some(2)));
        assert_eq!(seen[2].0, ROUND);
        assert_eq!(seen[3], (LAYER, 0, 4096, Some(3)));
        assert!(read(&mut &bytes[..bytes.len() - 1], &mut |_| Ok(())).is_err());
    }

    /// Rounds that wrote no end, written on by another writer (a live move's last save),
    /// read back as one stream: its records after theirs, then its end.
    #[test]
    fn a_last_round_goes_on_from_the_rounds() {
        let mut bytes = Vec::new();
        {
            let mut w = Writer::new(&mut bytes).unwrap();
            w.record(RAM, 0, &[1; 8192]).unwrap();
            w.round(0).unwrap();
        }
        let mut last = Writer::going_on(&mut bytes);
        last.record(RAM, 0x1000, &[0; 4096]).unwrap();
        last.record(SWAP, 0x2000, &[4; 4096]).unwrap();
        last.finish().unwrap();
        let mut seen = Vec::new();
        read(&mut bytes.as_slice(), &mut |r| {
            seen.push((r.kind, r.offset, r.bytes.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (RAM, 0, 8192),
                (ROUND, 0, 16),
                (RAM, 0x1000, 4096),
                (SWAP, 0x2000, 4096)
            ]
        );
    }
}
