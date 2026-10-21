//! ext4 file systems read on the host as Linux 6.18 reads them (fs/ext4/super.c, inode.c,
//! extents.c, dir.c, namei.c, xattr.c and acl.c, at v6.18.48): a run's writable layer as
//! its guest left it (D190), read without a VM by `commit`, `diff`, `export` and `cp` of a
//! stopped container, and to put its files back as it starts again.
//!
//! Its bytes are its guest's, which no host trusts: every number is checked against the
//! file system before it is used, every structure that carries a checksum against it
//! (metadata_csum), and nothing read is taken for more than the file system holds. A file
//! system its kernel did not leave consistent, its journal holding what it had yet to write
//! in place ([`crate::journal::needs_replay`]), is refused: [`crate::journal::replay`]
//! replays it first, as a guest's kernel does as it mounts it.

use std::collections::HashMap;
use std::io;

use crate::{Error, crc32c, group_desc_csum, inode_csum, inode_seed};

/// Bytes read from a place in a file: what a volume is read from.
pub trait ReadAt {
    /// Fills `buf` from `offset` on: an error where the file ends first.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// Its length in bytes.
    fn size(&self) -> io::Result<u64>;
}

impl ReadAt for std::fs::File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::read_exact_at(self, buf, offset)
        }
        #[cfg(windows)]
        {
            let mut done = 0;
            while done < buf.len() {
                let at = offset
                    .checked_add(done as u64)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                let rest = buf.get_mut(done..).unwrap_or_default();
                match std::os::windows::fs::FileExt::seek_read(self, rest, at)? {
                    0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                    n => done += n,
                }
            }
            Ok(())
        }
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}

impl ReadAt for [u8] {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let at = usize::try_from(offset).map_err(|_| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        let src = at
            .checked_add(buf.len())
            .and_then(|end| self.get(at..end))
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        (**self).read_exact_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
}

impl ReadAt for Vec<u8> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.as_slice().read_exact_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }
}

fn corrupt<T>(what: impl std::fmt::Display) -> Result<T, Error> {
    Err(Error(format!("a damaged file system: {what}")))
}

/// Little-endian fields of a structure read from the file system, each checked to lie
/// within it.
struct Raw<'a>(&'a [u8]);

impl Raw<'_> {
    fn bytes(&self, at: usize, len: usize) -> Result<&[u8], Error> {
        at.checked_add(len)
            .and_then(|end| self.0.get(at..end))
            .map_or_else(
                || corrupt(format!("{len} bytes at {at} of a structure of {}", self.0.len())),
                Ok,
            )
    }

    fn u8(&self, at: usize) -> Result<u8, Error> {
        Ok(self.bytes(at, 1)?.first().copied().unwrap_or_default())
    }

    fn u16(&self, at: usize) -> Result<u16, Error> {
        let mut v = [0u8; 2];
        v.copy_from_slice(self.bytes(at, 2)?);
        Ok(u16::from_le_bytes(v))
    }

    fn u32(&self, at: usize) -> Result<u32, Error> {
        let mut v = [0u8; 4];
        v.copy_from_slice(self.bytes(at, 4)?);
        Ok(u32::from_le_bytes(v))
    }
}

/// Features a file system may have that this reads, and those it refuses (ext4.h).
mod feature {
    /// FILETYPE, RECOVER (refused with a journal to replay), EXTENTS, 64BIT, MMP, FLEX_BG,
    /// CSUM_SEED, LARGEDIR.
    pub const INCOMPAT_KNOWN: u32 = 0x2 | 0x4 | 0x40 | 0x80 | 0x100 | 0x200 | 0x2000 | 0x4000;
    pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
    pub const INCOMPAT_RECOVER: u32 = 0x4;
    pub const INCOMPAT_64BIT: u32 = 0x80;
    pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
    pub const RO_COMPAT_HUGE_FILE: u32 = 0x8;
    pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
}

/// `s_state`'s EXT4_ERROR_FS: its kernel found it damaged.
const STATE_ERRORS: u16 = 0x2;
const EXTENTS_FL: u32 = 0x8_0000;
/// EXT4_INDEX_FL: a directory with a hash tree, whose first block is its root.
const INDEX_FL: u32 = 0x1000;
const HUGE_FILE_FL: u32 = 0x4_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const EA_INODE_FL: u32 = 0x20_0000;
const XATTR_MAGIC: u32 = 0xEA02_0000;
/// EXT4_MAX_EXTENT_DEPTH.
const MAX_DEPTH: u16 = 5;
/// An unwritten extent's length is past this (EXT_INIT_MAX_LEN).
const INIT_MAX_LEN: u16 = 32768;

/// The root directory's inode.
pub const ROOT: u32 = 2;

/// A file's type, from its mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Char,
    Block,
    Fifo,
    Socket,
}

impl Kind {
    fn of(mode: u16) -> Result<Kind, Error> {
        Ok(match mode & 0o170_000 {
            0o100_000 => Kind::File,
            0o040_000 => Kind::Dir,
            0o120_000 => Kind::Symlink,
            0o020_000 => Kind::Char,
            0o060_000 => Kind::Block,
            0o010_000 => Kind::Fifo,
            0o140_000 => Kind::Socket,
            other => return corrupt(format!("a file of type {other:o}")),
        })
    }
}

/// A time as an inode keeps it: seconds since the epoch, and nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

/// What an inode says of its file.
#[derive(Debug, Clone)]
pub struct Inode {
    pub ino: u32,
    pub kind: Kind,
    /// `i_mode`: its type and permission bits.
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub links: u16,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
    /// The bytes it holds on the file system, its extended attributes' block among them,
    /// in 512-byte sectors (`st_blocks`).
    pub sectors: u64,
    pub(crate) flags: u32,
    generation: u32,
    block: [u8; 60],
    file_acl: u64,
    /// The bytes past the inode's first 128 that hold attributes: from its extra fields'
    /// end to the inode's.
    ibody: Vec<u8>,
}

impl Inode {
    /// The depth of its extent tree, 0 where its inode holds every extent.
    #[cfg(test)]
    pub(crate) fn depth(&self) -> u16 {
        u16::from_le_bytes([self.block[6], self.block[7]])
    }

    /// A device's numbers, major and minor, as the kernel decodes them (ext4_iget,
    /// old_decode_dev and new_decode_dev).
    pub fn device(&self) -> (u32, u32) {
        let word = |i: usize| {
            let mut v = [0u8; 4];
            if let Some(b) = self.block.get(i * 4..i * 4 + 4) {
                v.copy_from_slice(b);
            }
            u32::from_le_bytes(v)
        };
        let old = word(0);
        if old != 0 {
            return ((old >> 8) & 0xFF, old & 0xFF);
        }
        let new = word(1);
        ((new & 0xF_FF00) >> 8, (new & 0xFF) | ((new >> 12) & 0xF_FF00))
    }
}

/// Extended attributes, each a full name (`user.foo`) and its value, by name.
pub type Xattrs = Vec<(Vec<u8>, Vec<u8>)>;

/// A directory's entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: Vec<u8>,
    pub ino: u32,
    /// The type it says its inode is (EXT4_FT_*: 2 a directory), 0 where it says none.
    pub file_type: u8,
}

/// EXT4_FT_DIR.
const FT_DIR: u8 = 2;

/// A run of a file's blocks: `len` blocks from its block `logical` on, at `physical` of
/// the file system; unwritten ones read as zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    logical: u32,
    len: u32,
    physical: u64,
    unwritten: bool,
}

/// An ext4 file system, opened for reading.
pub struct Volume<F: ReadAt + ?Sized> {
    block: u64,
    blocks: u64,
    inodes: u32,
    ipg: u32,
    inode_size: u16,
    /// Each group's inode table's first block.
    tables: Vec<u64>,
    csum_seed: Option<u32>,
    huge_files: bool,
    /// Marked as needing recovery (`needs_recovery`), its journal's inode (0 for none).
    recover: bool,
    journal: u32,
    file: F,
}

impl<F: ReadAt + ?Sized> std::fmt::Debug for Volume<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("block", &self.block)
            .field("blocks", &self.blocks)
            .field("inodes", &self.inodes)
            .finish_non_exhaustive()
    }
}

impl<F: ReadAt> Volume<F> {
    /// The file system `file` holds from its byte 0: refused where its kernel did not
    /// leave it consistent, its journal holding what it had yet to write in place, where it
    /// found it damaged, and where it has a feature this does not read. One marked as
    /// needing recovery whose journal is empty (jbd2_journal_flush, as EXT4_IOC_CHECKPOINT
    /// leaves it) is as its kernel would find it, with nothing to replay.
    pub fn open(file: F) -> Result<Volume<F>, Error> {
        let volume = Volume::open_any(file)?;
        if volume.recover && crate::journal::log_start(&volume)? != 0 {
            return Err(Error(
                "its journal holds what its kernel had yet to write in place: replay it first".into(),
            ));
        }
        Ok(volume)
    }

    /// [`open`](Self::open), one left with its journal to replay too: for the replay
    /// ([`crate::journal`]), which reads its journal through it.
    pub(crate) fn open_any(file: F) -> Result<Volume<F>, Error> {
        let mut sb = [0u8; 1024];
        file.read_exact_at(&mut sb, 1024)
            .map_err(|e| Error(format!("its superblock: {e}")))?;
        let s = Raw(&sb);
        if s.u16(0x38)? != crate::MAGIC {
            return Err(Error(
                "no ext4 file system: its superblock's magic is not 0xEF53".into(),
            ));
        }
        let incompat = s.u32(0x60)?;
        let ro_compat = s.u32(0x64)?;
        let csum = ro_compat & feature::RO_COMPAT_METADATA_CSUM != 0;
        if csum {
            if s.u8(0x175)? != crate::CRC32C_CHKSUM {
                return corrupt("its superblock's checksum is not crc32c");
            }
            if crc32c(!0, s.bytes(0, 0x3FC)?) != s.u32(0x3FC)? {
                return corrupt("its superblock's checksum");
            }
        }
        if s.u32(0x4C)? < 1 {
            return Err(Error(
                "an ext2 file system of the first revision: not ext4".into(),
            ));
        }
        let unknown = incompat & !feature::INCOMPAT_KNOWN;
        if unknown != 0 {
            return Err(Error(format!(
                "an ext4 file system with features this does not read (incompat {unknown:#x})"
            )));
        }
        let recover = incompat & feature::INCOMPAT_RECOVER != 0;
        let journal = if s.u32(0x5C)? & feature::COMPAT_HAS_JOURNAL != 0 {
            s.u32(0xE0)?
        } else {
            0
        };
        if s.u16(0x3A)? & STATE_ERRORS != 0 {
            return corrupt("its kernel found errors in it");
        }
        let log = s.u32(0x18)?;
        if log > 6 {
            return corrupt(format!("blocks of 1024 << {log} bytes"));
        }
        let block = 1024u64 << log;
        let bit64 = incompat & feature::INCOMPAT_64BIT != 0;
        let mut blocks = u64::from(s.u32(0x4)?);
        if bit64 {
            blocks |= u64::from(s.u32(0x150)?) << 32;
        }
        let inodes = s.u32(0x0)?;
        let bpg = u64::from(s.u32(0x20)?);
        let ipg = s.u32(0x28)?;
        let first = u64::from(s.u32(0x14)?);
        let inode_size = s.u16(0x58)?;
        if bpg == 0 || bpg > 8 * block || ipg == 0 || u64::from(ipg) > 8 * block {
            return corrupt(format!("groups of {bpg} blocks and {ipg} inodes"));
        }
        if inode_size < 128 || !inode_size.is_power_of_two() || u64::from(inode_size) > block {
            return corrupt(format!("inodes of {inode_size} bytes"));
        }
        if first >= blocks {
            return corrupt(format!("its first block {first} of {blocks}"));
        }
        // No more than its file holds, as the kernel holds one to its device (ext4_fill_super,
        // "block count exceeds size of device"), nor than a layer may be: what this reads
        // is what `format` makes, and a guest never grows it.
        let file_len = file.size().map_err(|e| Error(format!("its file: {e}")))?;
        if blocks > crate::MOST_BLOCKS || blocks.saturating_mul(block) > file_len {
            return corrupt(format!(
                "{blocks} blocks of {block} bytes in a file of {file_len}"
            ));
        }
        let groups = (blocks - first).div_ceil(bpg);
        if u64::from(inodes) != groups.saturating_mul(u64::from(ipg)) {
            return corrupt(format!("{inodes} inodes in {groups} groups of {ipg}"));
        }
        let desc = if bit64 { u64::from(s.u16(0xFE)?) } else { 32 };
        if desc < 32 || (bit64 && desc < 64) || desc > block || !desc.is_power_of_two() {
            return corrupt(format!("group descriptors of {desc} bytes"));
        }
        let csum_seed = csum.then(|| -> Result<u32, Error> {
            Ok(if incompat & feature::INCOMPAT_CSUM_SEED != 0 {
                s.u32(0x270)?
            } else {
                crc32c(!0, s.bytes(0x68, 16)?)
            })
        });
        let csum_seed = csum_seed.transpose()?;
        // The descriptors, after the block that holds the superblock, within its first group
        // (no meta_bg, which this refuses).
        let table_len = groups
            .checked_mul(desc)
            .filter(|&b| b.div_ceil(block) < bpg)
            .and_then(|b| usize::try_from(b).ok())
            .ok_or_else(|| Error(format!("a damaged file system: {groups} groups")))?;
        let at = (first + 1)
            .checked_mul(block)
            .ok_or_else(|| Error("a damaged file system: its descriptors past any file".into()))?;
        let mut table = vec![0u8; table_len];
        file.read_exact_at(&mut table, at)
            .map_err(|e| Error(format!("its group descriptors: {e}")))?;
        let itb = (u64::from(ipg) * u64::from(inode_size)).div_ceil(block);
        let mut tables = Vec::with_capacity(usize::try_from(groups).unwrap_or_default());
        for (group, d) in table.chunks_exact(desc as usize).enumerate() {
            let d = Raw(d);
            if let Some(seed) = csum_seed
                && group_desc_csum(seed, group as u64, d.bytes(0, desc as usize)?)? != d.u16(0x1E)?
            {
                return corrupt(format!("group {group}'s descriptor's checksum"));
            }
            let mut itable = u64::from(d.u32(0x8)?);
            if bit64 {
                itable |= u64::from(d.u32(0x28)?) << 32;
            }
            if itable.checked_add(itb).is_none_or(|end| end > blocks) || itable <= first {
                return corrupt(format!("group {group}'s inode table at block {itable}"));
            }
            tables.push(itable);
        }
        Ok(Volume {
            block,
            blocks,
            inodes,
            ipg,
            inode_size,
            tables,
            csum_seed,
            huge_files: ro_compat & feature::RO_COMPAT_HUGE_FILE != 0,
            recover,
            journal,
            file,
        })
    }

    /// Whether it is marked as needing recovery (`needs_recovery`), as its kernel leaves
    /// it while it is mounted.
    pub(crate) fn recovering(&self) -> bool {
        self.recover
    }

    /// Its journal's inode: 0 where it has none.
    pub(crate) fn journal_inode(&self) -> u32 {
        self.journal
    }

    /// Its blocks' size, in bytes.
    pub fn block_size(&self) -> u64 {
        self.block
    }

    /// How many blocks it has.
    pub(crate) fn blocks(&self) -> u64 {
        self.blocks
    }

    /// `inode`'s runs of blocks: from each one's first logical block, how many, where they
    /// are, and whether they are unwritten.
    pub(crate) fn extent_runs(&self, inode: &Inode) -> Result<Vec<(u64, u64, u64, bool)>, Error> {
        Ok(self
            .extents(inode)?
            .into_iter()
            .map(|e| (u64::from(e.logical), u64::from(e.len), e.physical, e.unwritten))
            .collect())
    }

    /// Block `n` of the file system.
    fn read_block(&self, n: u64) -> Result<Vec<u8>, Error> {
        if n >= self.blocks {
            return corrupt(format!("block {n} of {}", self.blocks));
        }
        let mut b = vec![0u8; self.block as usize];
        self.file
            .read_exact_at(&mut b, n * self.block)
            .map_err(|e| Error(format!("block {n}: {e}")))?;
        Ok(b)
    }

    /// Inode `ino`, which must be in use: one a directory names.
    pub fn inode(&self, ino: u32) -> Result<Inode, Error> {
        if ino == 0 || ino > self.inodes {
            return corrupt(format!("inode {ino} of {}", self.inodes));
        }
        let index = ino - 1;
        let table = self
            .tables
            .get((index / self.ipg) as usize)
            .copied()
            .ok_or_else(|| Error(format!("a damaged file system: inode {ino}'s group")))?;
        let at = table * self.block + u64::from(index % self.ipg) * u64::from(self.inode_size);
        let mut raw = vec![0u8; usize::from(self.inode_size)];
        self.file
            .read_exact_at(&mut raw, at)
            .map_err(|e| Error(format!("inode {ino}: {e}")))?;
        let r = Raw(&raw);
        let extra = if self.inode_size > 128 { r.u16(0x80)? } else { 0 };
        if 128 + usize::from(extra) > raw.len() || !extra.is_multiple_of(4) {
            return corrupt(format!("inode {ino}'s extra size {extra}"));
        }
        if let Some(seed) = self.csum_seed {
            let mut said = u32::from(r.u16(0x7C)?);
            if extra >= 4 {
                said |= u32::from(r.u16(0x82)?) << 16;
            }
            let mut sum = inode_csum(seed, ino, &raw)?;
            if extra < 4 {
                sum &= 0xFFFF;
            }
            if sum != said {
                return corrupt(format!("inode {ino}'s checksum"));
            }
        }
        let mode = r.u16(0x0)?;
        let links = r.u16(0x1A)?;
        if links == 0 {
            return corrupt(format!("inode {ino} is named but not in use"));
        }
        let flags = r.u32(0x20)?;
        if flags & (INLINE_DATA_FL | EA_INODE_FL) != 0 {
            return corrupt(format!("inode {ino}'s flags {flags:#x}"));
        }
        let fits = |field: usize| 128 + usize::from(extra) >= field + 4;
        let time = |base: usize, extra_at: usize| -> Result<Time, Error> {
            let sec = i64::from(r.u32(base)? as i32);
            if !fits(extra_at) {
                return Ok(Time { sec, nsec: 0 });
            }
            let x = r.u32(extra_at)?;
            Ok(Time {
                sec: sec + (i64::from(x & 3) << 32),
                nsec: x >> 2,
            })
        };
        let mut block = [0u8; 60];
        block.copy_from_slice(r.bytes(0x28, 60)?);
        let blocks_lo = u64::from(r.u32(0x1C)?);
        let blocks = blocks_lo | (u64::from(r.u16(0x74)?) << 32);
        let sectors = if self.huge_files && flags & HUGE_FILE_FL != 0 {
            blocks.saturating_mul(self.block / 512)
        } else {
            blocks
        };
        let ibody_at = 128 + usize::from(extra);
        Ok(Inode {
            ino,
            kind: Kind::of(mode)?,
            mode,
            uid: u32::from(r.u16(0x2)?) | (u32::from(r.u16(0x78)?) << 16),
            gid: u32::from(r.u16(0x18)?) | (u32::from(r.u16(0x7A)?) << 16),
            size: u64::from(r.u32(0x4)?) | (u64::from(r.u32(0x6C)?) << 32),
            links,
            atime: time(0x8, 0x8C)?,
            mtime: time(0x10, 0x88)?,
            ctime: time(0xC, 0x84)?,
            sectors,
            flags,
            generation: r.u32(0x64)?,
            block,
            file_acl: u64::from(r.u32(0x68)?) | (u64::from(r.u16(0x76)?) << 32),
            ibody: raw.get(ibody_at..).unwrap_or_default().to_vec(),
        })
    }

    /// The checksum seed of `inode`'s own structures: its extent tree's blocks, its
    /// directory's.
    fn seed_of(&self, inode: &Inode) -> Option<u32> {
        self.csum_seed
            .map(|seed| inode_seed(seed, inode.ino, inode.generation))
    }

    /// `inode`'s extents, in the order of its blocks.
    fn extents(&self, inode: &Inode) -> Result<Vec<Extent>, Error> {
        if inode.flags & EXTENTS_FL == 0 {
            if inode.size == 0 || matches!(inode.kind, Kind::Char | Kind::Block | Kind::Fifo | Kind::Socket) {
                return Ok(Vec::new());
            }
            return Err(Error(format!(
                "inode {}: blocks mapped without extents, which this does not read",
                inode.ino
            )));
        }
        let mut out = Vec::new();
        // Nodes to read: each one's bytes, its depth as its parent says (none for the
        // root's), and the logical blocks it maps, from its parent's index on.
        let mut stack: Vec<(Vec<u8>, Option<u16>, u64, u64)> = vec![(inode.block.to_vec(), None, 0, 1 << 32)];
        let seed = self.seed_of(inode);
        // Every node a block of the file system, read once: a tree holds no more.
        let mut nodes = 0u64;
        while let Some((node, want, from, to)) = stack.pop() {
            let n = Raw(&node);
            if n.u16(0)? != crate::EXTENT_MAGIC {
                return corrupt(format!("inode {}'s extent tree", inode.ino));
            }
            let (entries, max, depth) = (n.u16(2)?, n.u16(4)?, n.u16(6)?);
            let room = (node.len() - 12) / 12;
            if entries > max
                || usize::from(max) > room
                || depth > MAX_DEPTH
                || want.is_some_and(|w| w != depth)
            {
                return corrupt(format!("inode {}'s extent tree's node", inode.ino));
            }
            // ext4_valid_extent_entries: a node below the root starts where its index
            // says; entries in order, leaves' runs apart, all short of the next index.
            let mut children = Vec::new();
            let mut cur = from;
            for i in 0..usize::from(entries) {
                let e = Raw(n.bytes(12 + i * 12, 12)?);
                let logical = u64::from(e.u32(0)?);
                if (i == 0 && want.is_some() && logical != from) || logical < cur || logical >= to {
                    return corrupt(format!("inode {}'s extents out of order", inode.ino));
                }
                if depth == 0 {
                    let raw_len = e.u16(4)?;
                    let (len, unwritten) = if raw_len > INIT_MAX_LEN {
                        (raw_len - INIT_MAX_LEN, true)
                    } else {
                        (raw_len, false)
                    };
                    let physical = (u64::from(e.u16(6)?) << 32) | u64::from(e.u32(8)?);
                    let end = logical + u64::from(len);
                    if len == 0
                        || end > to
                        || physical
                            .checked_add(u64::from(len))
                            .is_none_or(|p| p > self.blocks)
                    {
                        return corrupt(format!("inode {}'s extent at its block {logical}", inode.ino));
                    }
                    cur = end;
                    out.push(Extent {
                        logical: logical as u32,
                        len: u32::from(len),
                        physical,
                        unwritten,
                    });
                } else {
                    let leaf = (u64::from(e.u16(8)?) << 32) | u64::from(e.u32(4)?);
                    children.push((logical, leaf));
                    cur = logical + 1;
                }
            }
            // A child's blocks are its own from its index's start to the next's.
            for (i, &(logical, leaf)) in children.iter().enumerate().rev() {
                let end = children.get(i + 1).map_or(to, |&(next, _)| next);
                nodes += 1;
                if nodes > self.blocks {
                    return corrupt(format!("inode {}'s extent tree", inode.ino));
                }
                let child = self.read_block(leaf)?;
                if let Some(seed) = seed {
                    let c = Raw(&child);
                    let tail = 12 + 12 * usize::from(c.u16(4)?);
                    if crc32c(seed, c.bytes(0, tail)?) != c.u32(tail)? {
                        return corrupt(format!("inode {}'s extent block {leaf}'s checksum", inode.ino));
                    }
                }
                stack.push((child, Some(depth - 1), logical, end));
            }
        }
        out.sort_by_key(|e| e.logical);
        if out.windows(2).any(|w| match w {
            [a, b] => u64::from(a.logical) + u64::from(a.len) > u64::from(b.logical),
            _ => false,
        }) {
            return corrupt(format!("inode {}'s extents overlap", inode.ino));
        }
        Ok(out)
    }

    /// `inode`'s bytes from `offset` into `buf`, the rest of `buf` past its end untouched:
    /// how many were read. Holes and unwritten extents read as zeros.
    pub fn read_at(&self, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let extents = self.extents(inode)?;
        self.read_with(&extents, inode.size, offset, buf)
    }

    fn read_with(&self, extents: &[Extent], size: u64, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        if offset >= size {
            return Ok(0);
        }
        let n = usize::try_from((size - offset).min(buf.len() as u64)).unwrap_or(buf.len());
        let out = buf.get_mut(..n).unwrap_or_default();
        out.fill(0);
        let (start, end) = (offset, offset + n as u64);
        // The first extent that ends past `start`: the rest are in order.
        let first =
            extents.partition_point(|e| (u64::from(e.logical) + u64::from(e.len)) * self.block <= start);
        for e in extents.get(first..).unwrap_or_default() {
            let from = u64::from(e.logical) * self.block;
            if from >= end {
                break;
            }
            let to = from + u64::from(e.len) * self.block;
            if e.unwritten {
                continue;
            }
            let (a, b) = (start.max(from), end.min(to));
            let dst = out
                .get_mut((a - start) as usize..(b - start) as usize)
                .unwrap_or_default();
            self.file
                .read_exact_at(dst, e.physical * self.block + (a - from))
                .map_err(|e| Error(format!("a file's data: {e}")))?;
        }
        Ok(n)
    }

    /// What reads `inode`'s bytes in order, holes as zeros.
    pub fn reader(&self, inode: &Inode) -> Result<Reader<'_, F>, Error> {
        Ok(Reader {
            volume: self,
            extents: self.extents(inode)?,
            size: inode.size,
            at: 0,
        })
    }

    /// The entries of directory `dir`, `.` and `..` left out, in the order its blocks keep
    /// them.
    pub fn read_dir(&self, dir: &Inode) -> Result<Vec<Entry>, Error> {
        if dir.kind != Kind::Dir {
            return Err(Error(format!("inode {} is no directory", dir.ino)));
        }
        if !dir.size.is_multiple_of(self.block) {
            return corrupt(format!("directory {} of {} bytes", dir.ino, dir.size));
        }
        let extents = self.extents(dir)?;
        let seed = self.seed_of(dir);
        let bs = self.block as usize;
        let mut out = Vec::new();
        let mut buf = vec![0u8; bs];
        for b in 0..dir.size / self.block {
            self.read_with(&extents, dir.size, b * self.block, &mut buf)?;
            let raw = Raw(&buf);
            // A node of its hash tree, behind a fake entry spanning the block, holds no
            // names (dx_node).
            if raw.u32(0)? == 0 && rec_len(raw.u16(4)?, bs) == bs {
                continue;
            }
            // A leaf's last 12 bytes: a fake entry holding its checksum
            // (ext4_dir_entry_tail).
            let tail = raw.u32(bs - 12)? == 0
                && raw.u16(bs - 8)? == 12
                && raw.u8(bs - 6)? == 0
                && raw.u8(bs - 5)? == 0xDE;
            let end = if tail { bs - 12 } else { bs };
            if let Some(seed) = seed {
                if tail {
                    if crc32c(seed, raw.bytes(0, bs - 12)?) != raw.u32(bs - 4)? {
                        return corrupt(format!("directory {}'s block {b}'s checksum", dir.ino));
                    }
                } else if b != 0 || dir.flags & INDEX_FL == 0 {
                    // Only a hash tree's root, whose `..` spans the rest of its block,
                    // holds no checksum of a leaf's.
                    return corrupt(format!("directory {}'s block {b} has no checksum", dir.ino));
                }
            }
            let mut at = 0;
            while at < end {
                let e = Raw(raw.bytes(at, 8)?);
                let ino = e.u32(0)?;
                let len = rec_len(e.u16(4)?, bs);
                let name_len = usize::from(e.u8(6)?);
                // ext4_check_dir_entry.
                if len < 12
                    || !len.is_multiple_of(4)
                    || len < (8 + name_len).next_multiple_of(4)
                    || at + len > end
                {
                    return corrupt(format!("directory {}'s entry at {at} of its block {b}", dir.ino));
                }
                if ino != 0 {
                    if ino > self.inodes {
                        return corrupt(format!("directory {} names inode {ino}", dir.ino));
                    }
                    let name = raw.bytes(at + 8, name_len)?;
                    if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
                        return corrupt(format!(
                            "directory {} names {:?}",
                            dir.ino,
                            String::from_utf8_lossy(name)
                        ));
                    }
                    if name != b"." && name != b".." {
                        out.push(Entry {
                            name: name.to_vec(),
                            ino,
                            file_type: e.u8(7)?,
                        });
                    }
                }
                at += len;
            }
        }
        Ok(out)
    }

    /// The target of symbolic link `link`.
    pub fn read_link(&self, link: &Inode) -> Result<Vec<u8>, Error> {
        if link.kind != Kind::Symlink {
            return Err(Error(format!("inode {} is no symbolic link", link.ino)));
        }
        let size = usize::try_from(link.size).unwrap_or(usize::MAX);
        if size == 0 || link.size > self.block {
            return corrupt(format!("symbolic link {} of {} bytes", link.ino, link.size));
        }
        // Its target in the inode where no block is its (ext4_inode_is_fast_symlink): the
        // blocks it holds are its attributes' alone.
        let acl = if link.file_acl != 0 { self.block / 512 } else { 0 };
        if link.sectors.saturating_sub(acl) == 0 {
            if size >= link.block.len() {
                return corrupt(format!("symbolic link {} of {size} bytes in its inode", link.ino));
            }
            return Ok(link.block.get(..size).unwrap_or_default().to_vec());
        }
        let mut target = vec![0u8; size];
        let n = self.read_at(link, 0, &mut target)?;
        target.truncate(n);
        Ok(target)
    }

    /// `inode`'s extended attributes by their full names (`user.foo`), from the inode's
    /// own bytes and its block, POSIX ACLs as the VFS gives them (acl.c).
    pub fn xattrs(&self, inode: &Inode) -> Result<Xattrs, Error> {
        let mut out = Vec::new();
        let body = Raw(&inode.ibody);
        if inode.ibody.len() >= 4 && body.u32(0)? == XATTR_MAGIC {
            let region = inode.ibody.get(4..).unwrap_or_default();
            entries(region, region, inode.ino, &mut out)?;
        }
        if inode.file_acl != 0 {
            let block = self.read_block(inode.file_acl)?;
            let b = Raw(&block);
            if b.u32(0)? != XATTR_MAGIC || b.u32(8)? != 1 {
                return corrupt(format!("inode {}'s attributes' block", inode.ino));
            }
            if let Some(seed) = self.csum_seed {
                let mut sum = crc32c(seed, &inode.file_acl.to_le_bytes());
                sum = crc32c(sum, b.bytes(0, 16)?);
                sum = crc32c(sum, &[0; 4]);
                sum = crc32c(sum, b.bytes(20, block.len() - 20)?);
                if sum != b.u32(16)? {
                    return corrupt(format!("inode {}'s attributes' block's checksum", inode.ino));
                }
            }
            entries(block.get(32..).unwrap_or_default(), &block, inode.ino, &mut out)?;
        }
        for (name, value) in &mut out {
            if name.as_slice() == b"system.posix_acl_access" || name.as_slice() == b"system.posix_acl_default"
            {
                *value = acl_to_xattr(value, inode.ino)?;
            }
        }
        out.sort();
        Ok(out)
    }

    /// The inode `path` names from the root, its names separated by `/`, `.` and empty
    /// names taken as none, no link followed; None where nothing has the name.
    pub fn lookup(&self, path: &[u8]) -> Result<Option<Inode>, Error> {
        let mut at = self.inode(ROOT)?;
        let mut seen = vec![ROOT];
        for name in path.split(|&c| c == b'/').filter(|n| !n.is_empty() && *n != b".") {
            if name == b".." {
                return Err(Error("a path with `..`, which names no file of this one".into()));
            }
            if at.kind != Kind::Dir {
                return Ok(None);
            }
            let Some(entry) = self.read_dir(&at)?.into_iter().find(|e| e.name == name) else {
                return Ok(None);
            };
            if seen.contains(&entry.ino) {
                return corrupt(format!("directory {} is its own ancestor", entry.ino));
            }
            seen.push(entry.ino);
            at = self.inode(entry.ino)?;
        }
        Ok(Some(at))
    }
}

/// A directory entry's length as it keeps it (ext4_rec_len_from_disk): blocks of 64 KiB
/// keep theirs in 16 bits with its low two bits holding the high.
fn rec_len(raw: u16, block: usize) -> usize {
    if block < 65536 {
        return usize::from(raw);
    }
    if raw == u16::MAX || raw == 0 {
        return block;
    }
    usize::from(raw & 0xFFFC) | (usize::from(raw & 3) << 16)
}

/// A file's bytes, in order: [`Volume::reader`].
pub struct Reader<'a, F: ReadAt + ?Sized> {
    volume: &'a Volume<F>,
    extents: Vec<Extent>,
    size: u64,
    at: u64,
}

impl<F: ReadAt + ?Sized> std::fmt::Debug for Reader<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("size", &self.size)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl<F: ReadAt> io::Read for Reader<'_, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self
            .volume
            .read_with(&self.extents, self.size, self.at, buf)
            .map_err(|e| io::Error::other(e.0))?;
        self.at += n as u64;
        Ok(n)
    }
}

/// Attribute entries from `region` on (ext4_xattr_entry), values at their offsets from
/// `values`'s start: the inode's body, whose values count from its first entry, or the
/// attributes' block, whose count from its own start.
fn entries(region: &[u8], values: &[u8], ino: u32, out: &mut Xattrs) -> Result<(), Error> {
    let r = Raw(region);
    let values = Raw(values);
    let mut at = 0;
    loop {
        // The last is followed by four zero bytes.
        if region.len() < at + 4 || r.u32(at)? == 0 {
            return Ok(());
        }
        let name_len = usize::from(r.u8(at)?);
        let index = r.u8(at + 1)?;
        let value_at = usize::from(r.u16(at + 2)?);
        let value_inum = r.u32(at + 4)?;
        let value_len = r.u32(at + 8)? as usize;
        let name = r.bytes(at + 16, name_len)?;
        // Its index's prefix, and whether that is its whole name, as an ACL's is.
        let (prefix, whole): (&[u8], bool) = match index {
            1 => (b"user.", false),
            2 => (b"system.posix_acl_access", true),
            3 => (b"system.posix_acl_default", true),
            4 => (b"trusted.", false),
            6 => (b"security.", false),
            7 => (b"system.", false),
            8 => (b"system.richacl", true),
            _ => return corrupt(format!("inode {ino}'s attribute of index {index}")),
        };
        if (name_len == 0) != whole || value_inum != 0 {
            return corrupt(format!("inode {ino}'s attribute at {at}"));
        }
        let value = values.bytes(value_at, value_len)?;
        out.push(([prefix, name].concat(), value.to_vec()));
        at += (16 + name_len).next_multiple_of(4);
    }
}

/// An ACL as ext4 keeps it (ext4_acl_header and ext4_acl_entry, acl.c's
/// ext4_acl_from_disk) as the VFS gives it (posix_acl_xattr_header, version 2, and entries
/// of eight bytes, ACL_UNDEFINED_ID for those of no user or group).
fn acl_to_xattr(disk: &[u8], ino: u32) -> Result<Vec<u8>, Error> {
    const USER: u16 = 0x02;
    const GROUP: u16 = 0x08;
    let r = Raw(disk);
    if r.u32(0)? != 1 {
        return corrupt(format!("inode {ino}'s ACL's version"));
    }
    let mut out = 2u32.to_le_bytes().to_vec();
    let mut at = 4;
    while at < disk.len() {
        let tag = r.u16(at)?;
        let perm = r.u16(at + 2)?;
        let (id, len) = match tag {
            USER | GROUP => (r.u32(at + 4)?, 8),
            0x01 | 0x04 | 0x10 | 0x20 => (u32::MAX, 4),
            _ => return corrupt(format!("inode {ino}'s ACL entry of tag {tag:#x}")),
        };
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&perm.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        at += len;
    }
    Ok(out)
}

/// What reads a tree of a volume by path, as a walk does: each directory's entries read
/// once while it is on the walk's way (`Volume::read_dir` scans a directory whole), and the
/// inodes they name.
pub struct Walker<F: ReadAt> {
    volume: Volume<F>,
    /// The entries of the directories read, by their inode, the most recent last: each
    /// name's inode and the type the directory says it is.
    listed: Vec<(u32, Listing)>,
}

/// A directory's entries by name: each one's inode and the type the directory says it is.
type Listing = HashMap<Vec<u8>, (u32, u8)>;

impl<F: ReadAt> std::fmt::Debug for Walker<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walker")
            .field("volume", &self.volume)
            .field("listed", &self.listed.len())
            .finish()
    }
}

/// How many directories' entries a [`Walker`] keeps: more than a walk's depth reaches.
const LISTED: usize = 64;

impl<F: ReadAt> Walker<F> {
    pub fn new(volume: Volume<F>) -> Walker<F> {
        Walker {
            volume,
            listed: Vec::new(),
        }
    }

    pub fn volume(&self) -> &Volume<F> {
        &self.volume
    }

    /// The entries of directory `dir` by name.
    fn names(&mut self, dir: &Inode) -> Result<&Listing, Error> {
        if let Some(i) = self.listed.iter().position(|(ino, _)| *ino == dir.ino) {
            let found = self.listed.remove(i);
            self.listed.push(found);
        } else {
            let names = self
                .volume
                .read_dir(dir)?
                .into_iter()
                .map(|e| (e.name, (e.ino, e.file_type)))
                .collect();
            if self.listed.len() >= LISTED {
                self.listed.remove(0);
            }
            self.listed.push((dir.ino, names));
        }
        self.listed
            .last()
            .map(|(_, names)| names)
            .ok_or_else(|| Error("no directory listed".into()))
    }

    /// The inode `path` names, as [`Volume::lookup`] finds it.
    pub fn lookup(&mut self, path: &[u8]) -> Result<Option<Inode>, Error> {
        let mut at = self.volume.inode(ROOT)?;
        let mut seen = vec![ROOT];
        for name in path.split(|&c| c == b'/').filter(|n| !n.is_empty() && *n != b".") {
            if name == b".." {
                return Err(Error("a path with `..`, which names no file of this one".into()));
            }
            if at.kind != Kind::Dir {
                return Ok(None);
            }
            let Some(&(ino, _)) = self.names(&at)?.get(name) else {
                return Ok(None);
            };
            if seen.contains(&ino) {
                return corrupt(format!("directory {ino} is its own ancestor"));
            }
            seen.push(ino);
            at = self.volume.inode(ino)?;
        }
        Ok(Some(at))
    }

    /// The names in directory `path`, sorted, each with whether it is a directory, as the
    /// directory says: a walk's, as Go's filepath.WalkDir takes a file's type from its
    /// directory (d_type). One that says none is asked of its inode.
    pub fn list(&mut self, path: &[u8]) -> Result<Vec<(Vec<u8>, bool)>, Error> {
        let dir = self
            .lookup(path)?
            .ok_or_else(|| Error(format!("{}: no such directory", String::from_utf8_lossy(path))))?;
        let names: Vec<(Vec<u8>, (u32, u8))> =
            self.names(&dir)?.iter().map(|(n, &e)| (n.clone(), e)).collect();
        let mut out = Vec::with_capacity(names.len());
        for (name, (ino, file_type)) in names {
            let is_dir = match file_type {
                0 => self.volume.inode(ino)?.kind == Kind::Dir,
                t => t == FT_DIR,
            };
            out.push((name, is_dir));
        }
        out.sort();
        Ok(out)
    }
}
