//! A file system's journal replayed on the host, as Linux 6.18's jbd2 replays one as it
//! mounts its file system (fs/jbd2/recovery.c, jbd2_journal_recover, at v6.18.48), and as
//! e2fsck and debugfs then mark it replayed (lib/ext2fs, ext2fs_journal_release): a run's
//! writable layer whose microVM went before its guest wrote its journal in place (D190),
//! so that it reads whole on the host.
//!
//! The log is walked three times from its start: to find its last whole transaction (a
//! commit block whose checksum holds), then for the blocks its revoke records take back,
//! then to write each block it logged in place, in order, but those a later transaction
//! revoked. A block written is whole as its transaction committed it: its checksum is
//! checked first. What the guest wrote and never committed is no transaction's, and stays
//! out, as it does when its kernel mounts it. Then, once what it wrote is in its file
//! ([`WriteAt::sync`]), the journal is marked empty, its next transaction's number after
//! the last, and the file system as needing no recovery: a host that stops partway leaves
//! the journal to replay again, as jbd2 leaves a device (sync_blockdev before the journal
//! is marked empty).
//!
//! Its bytes are its guest's: every block number is checked against the file system and
//! its journal, the walk ends within the journal's length, and a journal this does not read
//! (fast commits, asynchronous commits, version 2 checksums) is refused, for a guest's
//! kernel to replay.

use std::collections::HashMap;
use std::io;

use crate::read::{ReadAt, Volume};
use crate::{Error, crc32c};

/// Bytes written at a place in a file: what a journal is replayed into.
pub trait WriteAt {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Has what was written reach the file's storage before anything written after.
    fn sync(&self) -> io::Result<()>;
}

impl WriteAt for std::fs::File {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::write_all_at(self, buf, offset)
        }
        #[cfg(windows)]
        {
            let mut done = 0;
            while let Some(rest) = buf.get(done..).filter(|r| !r.is_empty()) {
                let at = offset
                    .checked_add(done as u64)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                match std::os::windows::fs::FileExt::seek_write(self, rest, at)? {
                    0 => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                    n => done += n,
                }
            }
            Ok(())
        }
    }

    fn sync(&self) -> io::Result<()> {
        self.sync_data()
    }
}

/// What a replay did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Replayed {
    /// The transactions it replayed: the log's whole ones.
    pub transactions: u32,
    /// The blocks it wrote in place.
    pub blocks: u64,
    /// The logged blocks a later transaction revoked, left out.
    pub revoked: u64,
}

const MAGIC: u32 = 0xC03B_3998;
const DESCRIPTOR: u32 = 1;
const COMMIT: u32 = 2;
const SUPERBLOCK_V1: u32 = 3;
const SUPERBLOCK_V2: u32 = 4;
const REVOKE: u32 = 5;

const INCOMPAT_REVOKE: u32 = 0x1;
const INCOMPAT_64BIT: u32 = 0x2;
const INCOMPAT_CSUM_V3: u32 = 0x10;

const FLAG_ESCAPE: u32 = 1;
const FLAG_SAME_UUID: u32 = 2;
const FLAG_LAST_TAG: u32 = 8;

/// The ext4 superblock's `needs_recovery` (EXT4_FEATURE_INCOMPAT_RECOVER).
const RECOVER: u32 = 0x4;

fn corrupt<T>(what: impl std::fmt::Display) -> Result<T, Error> {
    Err(Error(format!("a damaged journal: {what}")))
}

fn be32(b: &[u8], at: usize) -> Result<u32, Error> {
    let mut v = [0u8; 4];
    v.copy_from_slice(
        at.checked_add(4)
            .and_then(|end| b.get(at..end))
            .ok_or_else(|| Error(format!("a damaged journal: 4 bytes at {at} of {}", b.len())))?,
    );
    Ok(u32::from_be_bytes(v))
}

fn be64(b: &[u8], at: usize) -> Result<u64, Error> {
    Ok((u64::from(be32(b, at)?) << 32) | u64::from(be32(b, at + 4)?))
}

/// `x` is `y` or after, as transaction numbers wrap (tid_geq).
fn tid_geq(x: u32, y: u32) -> bool {
    x.wrapping_sub(y) as i32 >= 0
}

/// The CRC-32C over `block` with its four bytes at `at` taken as zeros, from `seed`: how
/// jbd2 checks its superblock, descriptor, revoke and commit blocks.
fn csum_without(seed: u32, block: &[u8], at: usize) -> Result<u32, Error> {
    let before = block
        .get(..at)
        .ok_or_else(|| Error("a short journal block".into()))?;
    let after = block
        .get(at + 4..)
        .ok_or_else(|| Error("a short journal block".into()))?;
    Ok(crc32c(crc32c(crc32c(seed, before), &[0; 4]), after))
}

/// The journal of a file system, as replay reads it.
struct Journal<'a, F: ReadAt> {
    volume: &'a Volume<&'a F>,
    file: &'a F,
    block: u64,
    /// The journal inode's runs: its blocks from `logical`, `len` of them at `physical`.
    runs: Vec<(u64, u64, u64)>,
    /// `s_first` and `s_maxlen`: where its log begins and ends, in its own blocks.
    first: u64,
    last: u64,
    /// The checksum seed, with version 3 checksums (crc32c of its UUID).
    seed: Option<u32>,
    tag_bytes: usize,
    bit64: bool,
}

impl<F: ReadAt> Journal<'_, F> {
    /// Block `n` of the journal.
    fn read(&self, n: u64) -> Result<Vec<u8>, Error> {
        let i = self.runs.partition_point(|&(l, len, _)| l + len <= n);
        let &(l, _, p) = self
            .runs
            .get(i)
            .filter(|&&(l, len, _)| (l..l + len).contains(&n))
            .ok_or_else(|| Error(format!("a damaged journal: its block {n} is in no extent")))?;
        let mut b = vec![0u8; self.block as usize];
        self.file
            .read_exact_at(&mut b, (p + n - l) * self.block)
            .map_err(|e| Error(format!("journal block {n}: {e}")))?;
        Ok(b)
    }

    /// The log's next block after `n`, wrapping from its end to its first.
    fn next(&self, n: u64) -> u64 {
        if n + 1 >= self.last { self.first } else { n + 1 }
    }

    /// A descriptor's tags: each logged block's number in the file system, its flags and
    /// its checksum (version 3's, else none), in order.
    fn tags(&self, desc: &[u8]) -> Result<Vec<(u64, u32, u32)>, Error> {
        let tail = if self.seed.is_some() { 4 } else { 0 };
        let end = desc.len() - tail;
        let mut out = Vec::new();
        let mut at = 12;
        while at + self.tag_bytes <= end {
            let low = u64::from(be32(desc, at)?);
            let flags = be32(desc, at + 4)? & 0xFFFF;
            let (high, csum) = if self.seed.is_some() {
                (u64::from(be32(desc, at + 8)?), be32(desc, at + 12)?)
            } else if self.bit64 {
                (u64::from(be32(desc, at + 8)?), 0)
            } else {
                (0, 0)
            };
            let block = if self.bit64 { low | (high << 32) } else { low };
            // Without version 3, a flag word's high half is a 16-bit checksum (version 2,
            // refused), its low half the flags, as jbd2 reads them (be16 t_flags).
            out.push((block, flags, csum));
            at += self.tag_bytes;
            if flags & FLAG_SAME_UUID == 0 {
                at += 16;
            }
            if flags & FLAG_LAST_TAG != 0 {
                break;
            }
        }
        Ok(out)
    }

    /// Whether a descriptor or revoke block's tail checksum holds; true without checksums.
    fn tail_holds(&self, b: &[u8]) -> Result<bool, Error> {
        match self.seed {
            Some(seed) => Ok(csum_without(seed, b, b.len() - 4)? == be32(b, b.len() - 4)?),
            None => Ok(true),
        }
    }

    /// Whether a commit block's checksum holds (h_chksum[0], at 16); true without.
    fn commit_holds(&self, b: &[u8]) -> Result<bool, Error> {
        match self.seed {
            Some(seed) => Ok(csum_without(seed, b, 16)? == be32(b, 16)?),
            None => Ok(true),
        }
    }

    /// Whether logged block `data` of transaction `sequence` is as its tag's checksum says.
    fn data_holds(&self, data: &[u8], sequence: u32, csum: u32) -> bool {
        match self.seed {
            Some(seed) => crc32c(crc32c(seed, &sequence.to_be_bytes()), data) == csum,
            None => true,
        }
    }
}

/// Which of jbd2's passes over the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    Scan,
    Revoke,
    Replay,
}

/// A journal's superblock, checked: its log's bounds, its first transaction and where it
/// starts, its features, and its checksum seed with version 3 checksums.
struct Header {
    maxlen: u64,
    first: u64,
    sequence: u32,
    start: u64,
    compat: u32,
    incompat: u32,
    seed: Option<u32>,
}

/// The journal superblock `jsb` of a file system of `block`-byte blocks, checked as jbd2
/// loads one (journal_check_superblock): its magic and type, its block size, and its
/// checksum, where it has one (version 3's crc32c, `s_checksum`).
fn header(jsb: &[u8], block: u64) -> Result<Header, Error> {
    let blocktype = be32(jsb, 4)?;
    if be32(jsb, 0)? != MAGIC || !matches!(blocktype, SUPERBLOCK_V1 | SUPERBLOCK_V2) {
        return corrupt("its superblock");
    }
    if u64::from(be32(jsb, 0x0C)?) != block {
        return corrupt(format!(
            "blocks of {} bytes in a file system of {block}",
            be32(jsb, 0x0C)?
        ));
    }
    let (compat, incompat) = if blocktype == SUPERBLOCK_V2 {
        (be32(jsb, 0x24)?, be32(jsb, 0x28)?)
    } else {
        (0, 0)
    };
    let seed = if incompat & INCOMPAT_CSUM_V3 != 0 {
        let short = || Error("a short journal superblock".into());
        let sb_part = jsb.get(..1024).ok_or_else(short)?;
        if csum_without(!0, sb_part, 0xFC)? != be32(jsb, 0xFC)? {
            return corrupt("its superblock's checksum");
        }
        Some(crc32c(!0, jsb.get(0x30..0x40).ok_or_else(short)?))
    } else {
        None
    };
    Ok(Header {
        maxlen: u64::from(be32(jsb, 0x10)?),
        first: u64::from(be32(jsb, 0x14)?),
        sequence: be32(jsb, 0x18)?,
        start: u64::from(be32(jsb, 0x1C)?),
        compat,
        incompat,
        seed,
    })
}

/// Where the log of `volume`'s journal starts (`s_start`), its superblock checked: 0 where
/// it holds nothing to replay, as jbd2 finds one (jbd2_journal_recover, "No recovery
/// required"), emptied as its file system stood mounted (jbd2_journal_flush, as
/// EXT4_IOC_CHECKPOINT and a freeze leave it) or once replayed.
pub(crate) fn log_start<F: ReadAt>(volume: &Volume<F>) -> Result<u64, Error> {
    let jnum = volume.journal_inode();
    if jnum == 0 {
        return Err(Error("marked as needing recovery, with no journal".into()));
    }
    let jinode = volume.inode(jnum)?;
    let mut jsb = vec![0u8; 1024];
    if volume.read_at(&jinode, 0, &mut jsb)? < jsb.len() {
        return corrupt("a short journal superblock");
    }
    Ok(header(&jsb, volume.block_size())?.start)
}

/// Whether the journal of the file system `file` holds what its kernel had yet to write in
/// place, for [`replay`]: marked as needing recovery (`needs_recovery`), its log not empty.
pub fn needs_replay<F: ReadAt>(file: &F) -> Result<bool, Error> {
    let volume = Volume::open_any(file)?;
    Ok(volume.recovering() && log_start(&volume)? != 0)
}

/// Replays the journal of the ext4 file system `file` holds, where it was left needing it
/// (`needs_recovery`): what it did, or None where nothing needed it. Afterwards the file
/// system reads as its kernel would leave it once mounted, its journal empty.
pub fn replay<F: ReadAt + WriteAt>(file: &F) -> Result<Option<Replayed>, Error> {
    let volume = Volume::open_any(file)?;
    if !volume.recovering() {
        return Ok(None);
    }
    let jnum = volume.journal_inode();
    if jnum == 0 {
        return Err(Error("marked as needing recovery, with no journal".into()));
    }
    let jinode = volume.inode(jnum)?;
    let block = volume.block_size();
    let runs: Vec<(u64, u64, u64)> = volume
        .extent_runs(&jinode)?
        .into_iter()
        .filter(|&(_, _, _, unwritten)| !unwritten)
        .map(|(l, len, p, _)| (l, len, p))
        .collect();
    let mut journal = Journal {
        volume: &volume,
        file,
        block,
        runs,
        first: 1,
        last: 2,
        seed: None,
        tag_bytes: 8,
        bit64: false,
    };
    let jsb = journal.read(0)?;
    let Header {
        maxlen,
        first,
        sequence,
        start,
        compat,
        incompat,
        seed,
    } = header(&jsb, block)?;
    let jblocks = jinode.size / block;
    if first == 0 || first >= maxlen || maxlen > jblocks {
        return corrupt(format!("a log from {first} to {maxlen} in {jblocks} blocks"));
    }
    if compat != 0 || incompat & !(INCOMPAT_REVOKE | INCOMPAT_64BIT | INCOMPAT_CSUM_V3) != 0 {
        return Err(Error(format!(
            "a journal with features this does not replay (compat {compat:#x}, incompat {incompat:#x}): a guest mounts it first"
        )));
    }
    journal.first = first;
    journal.last = maxlen;
    journal.bit64 = incompat & INCOMPAT_64BIT != 0;
    journal.seed = seed;
    if seed.is_some() {
        journal.tag_bytes = 16;
    } else if journal.bit64 {
        journal.tag_bytes = 12;
    }
    let mut done = Replayed::default();
    let mut end = sequence;
    if start != 0 {
        if start < first || start >= maxlen {
            return corrupt(format!("its log starts at {start}, past {first}..{maxlen}"));
        }
        let mut revoked: HashMap<u64, u32> = HashMap::new();
        end = pass(
            &journal,
            Pass::Scan,
            start,
            sequence,
            None,
            &mut revoked,
            &mut done,
        )?;
        pass(
            &journal,
            Pass::Revoke,
            start,
            sequence,
            Some(end),
            &mut revoked,
            &mut done,
        )?;
        pass(
            &journal,
            Pass::Replay,
            start,
            sequence,
            Some(end),
            &mut revoked,
            &mut done,
        )?;
        done.transactions = end.wrapping_sub(sequence);
    }
    // The journal empty, its next transaction after the last replayed (e2fsprogs'
    // ext2fs_journal_release, as jbd2_journal_recover leaves j_transaction_sequence), once
    // what it replayed is in the file; one empty already stays as it is.
    if start != 0 {
        file.sync()
            .map_err(|e| Error(format!("what its journal held, written in place: {e}")))?;
        write_empty(&journal, jsb, end)?;
    }
    // And the file system as needing no recovery, its superblock as the log left it.
    let mut sb = [0u8; 1024];
    file.read_exact_at(&mut sb, 1024)
        .map_err(|e| Error(format!("its superblock: {e}")))?;
    let incompat = u32::from_le_bytes([sb[0x60], sb[0x61], sb[0x62], sb[0x63]]) & !RECOVER;
    sb[0x60..0x64].copy_from_slice(&incompat.to_le_bytes());
    let ro_compat = u32::from_le_bytes([sb[0x64], sb[0x65], sb[0x66], sb[0x67]]);
    if ro_compat & 0x400 != 0 {
        let sum = crc32c(!0, &sb[..0x3FC]);
        sb[0x3FC..0x400].copy_from_slice(&sum.to_le_bytes());
    }
    file.write_all_at(&sb, 1024)
        .map_err(|e| Error(format!("its superblock: {e}")))?;
    file.sync().map_err(|e| Error(format!("its superblock: {e}")))?;
    Ok(Some(done))
}

/// Writes the journal's superblock `jsb` back marked empty (`s_start` 0), its next
/// transaction the one after `end`, the first the log did not hold whole.
fn write_empty<F: ReadAt + WriteAt>(
    journal: &Journal<'_, F>,
    mut jsb: Vec<u8>,
    end: u32,
) -> Result<(), Error> {
    let put = |b: &mut [u8], at: usize, v: u32| {
        if let Some(field) = b.get_mut(at..at + 4) {
            field.copy_from_slice(&v.to_be_bytes());
        }
    };
    put(&mut jsb, 0x18, end.wrapping_add(1));
    put(&mut jsb, 0x1C, 0);
    if journal.seed.is_some() {
        let sum = csum_without(!0, jsb.get(..1024).unwrap_or_default(), 0xFC)?;
        put(&mut jsb, 0xFC, sum);
    }
    let at = journal
        .runs
        .first()
        .filter(|&&(l, _, _)| l == 0)
        .map(|&(_, _, p)| p * journal.block)
        .ok_or_else(|| Error("a damaged journal: its first block is in no extent".into()))?;
    journal
        .file
        .write_all_at(&jsb, at)
        .map_err(|e| Error(format!("its journal's superblock: {e}")))
}

/// One of jbd2's passes over the log from block `start`, its first transaction `sequence`
/// (do_one_pass): the scan finds where the whole transactions end, which the others are
/// given (`end`); returns where this one ended.
fn pass<F: ReadAt + WriteAt>(
    j: &Journal<'_, F>,
    pass: Pass,
    start: u64,
    sequence: u32,
    end: Option<u32>,
    revoked: &mut HashMap<u64, u32>,
    done: &mut Replayed,
) -> Result<u32, Error> {
    let mut at = start;
    let mut next_commit = sequence;
    // A checksum that failed in the scan: whether the commit after it shows the log stale
    // (an older commit's time) or damaged.
    let mut check_commit_time = false;
    let mut last_commit_time = 0u64;
    let mut walked = 0u64;
    let fs_blocks = j.volume.blocks();
    loop {
        if let Some(end) = end
            && tid_geq(next_commit, end)
        {
            break;
        }
        // The log is no longer than the journal: a walk past its length is a loop.
        walked += 1;
        if walked > j.last {
            return corrupt("a log longer than its journal");
        }
        let b = j.read(at)?;
        at = j.next(at);
        if be32(&b, 0)? != MAGIC || be32(&b, 8)? != next_commit {
            break;
        }
        match be32(&b, 4)? {
            DESCRIPTOR => {
                let holds = j.tail_holds(&b)?;
                if !holds {
                    if pass != Pass::Scan {
                        return corrupt(format!("the descriptor at its block {at}'s checksum"));
                    }
                    check_commit_time = true;
                }
                let tags = j.tags(&b)?;
                if pass != Pass::Replay {
                    for _ in &tags {
                        at = j.next(at);
                    }
                    continue;
                }
                for (fs_block, flags, csum) in tags {
                    let logged = at;
                    at = j.next(at);
                    let mut data = j.read(logged)?;
                    if revoked.get(&fs_block).is_some_and(|&by| tid_geq(by, next_commit)) {
                        done.revoked += 1;
                        continue;
                    }
                    if !j.data_holds(&data, next_commit, csum) {
                        return corrupt(format!(
                            "the block of {fs_block} it logged at its block {logged}: its checksum"
                        ));
                    }
                    if fs_block >= fs_blocks {
                        return corrupt(format!("it logged block {fs_block} of {fs_blocks}"));
                    }
                    if flags & FLAG_ESCAPE != 0
                        && let Some(head) = data.get_mut(..4)
                    {
                        head.copy_from_slice(&MAGIC.to_be_bytes());
                    }
                    j.file
                        .write_all_at(&data, fs_block * j.block)
                        .map_err(|e| Error(format!("block {fs_block}: {e}")))?;
                    done.blocks += 1;
                }
            }
            COMMIT => {
                // h_commit_sec, after the checksums' fields.
                let time = be64(&b, 48)?;
                if pass == Pass::Scan {
                    if check_commit_time {
                        // A transaction after its log's last, stale, ends it; else the log
                        // is damaged where it was written.
                        if time >= last_commit_time {
                            return corrupt(format!("transaction {next_commit}'s checksums"));
                        }
                        break;
                    }
                    // A commit written in part, or a stale one: the transaction never was.
                    if !j.commit_holds(&b)? {
                        break;
                    }
                    last_commit_time = time;
                }
                next_commit = next_commit.wrapping_add(1);
            }
            REVOKE => {
                if pass == Pass::Scan && !j.tail_holds(&b)? {
                    check_commit_time = true;
                }
                if pass != Pass::Revoke {
                    continue;
                }
                // r_count: the bytes of the block its records reach, its header's among them.
                let count = be32(&b, 12)? as usize;
                let tail = if j.seed.is_some() { 4 } else { 0 };
                if count > b.len() - tail {
                    return corrupt(format!("revoke records of {count} bytes"));
                }
                let len = if j.bit64 { 8 } else { 4 };
                let mut r = 16;
                while r + len <= count {
                    let block = if j.bit64 {
                        be64(&b, r)?
                    } else {
                        u64::from(be32(&b, r)?)
                    };
                    let by = revoked.entry(block).or_insert(next_commit);
                    if tid_geq(next_commit, *by) {
                        *by = next_commit;
                    }
                    r += len;
                }
            }
            _ => break,
        }
    }
    Ok(next_commit)
}
