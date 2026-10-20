//! ext4 file systems as Linux 6.18 mounts them (fs/ext4/ext4.h, super.c, balloc.c,
//! bitmap.c, inode.c and namei.c, and include/linux/jbd2.h, at v6.18.48): a run's writable
//! layer, made empty on the host, which its guest mounts with DAX over pmem (D190).
//!
//! What it makes is what mke2fs 1.47.0 makes for ext4 by default (misc/mke2fs.conf.in:
//! has_journal, extent, huge_file, flex_bg, metadata_csum, 64bit, dir_nlink and extra_isize,
//! over sparse_super, large_file, filetype, dir_index and ext_attr), less what a layer has
//! no use for: no blocks kept to grow it by (resize_inode) or for root alone (`-m 0`), no
//! orphan file (orphan_file) and no checksum seed apart from its UUID (metadata_csum_seed).
//! 4 KiB blocks, 256-byte inodes, one inode to 16 KiB, groups of 32768 blocks in flex groups
//! of 16, and a journal of the size mke2fs gives one (lib/ext2fs/mkjournal.c,
//! ext2fs_default_journal_size), in one group.
//!
//! Only metadata is written; the rest of its file stays a hole, which reads as zeros. So
//! every group's inode table is zeroed as it stands (`EXT4_BG_INODE_ZEROED`), and neither
//! mke2fs's lazy init nor the kernel's (ext4lazyinit) ever writes one.

use std::io;

/// Why a file system could not be made: what went wrong, said.
#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error(e.to_string())
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error(msg.into()))
}

/// Its block size.
pub const BLOCK: u64 = 4096;
/// `s_log_block_size`: 1024 << 2.
const LOG_BLOCK: u32 = 2;
/// A group's blocks: the bits of one bitmap block.
const GROUP: u64 = 8 * BLOCK;
const INODE_SIZE: u64 = 256;
/// Bytes of the file system to each inode (mke2fs.conf's `inode_ratio` for ext4).
const INODE_RATIO: u64 = 16384;
const DESC_SIZE: u64 = 64;
/// Groups to a flex group, as a power of two.
const LOG_FLEX: u32 = 4;
const FLEX: u64 = 1 << LOG_FLEX;
/// The inode's bytes past the 128 of ext2's (`s_want_extra_isize`): through `i_projid`.
const EXTRA_ISIZE: u16 = 32;
const GOOD_OLD_INODE_SIZE: usize = 128;
/// The least a file system may be: where mke2fs makes no journal below it.
const LEAST_BLOCKS: u64 = 2048;
/// The most, in blocks: what 32-bit block numbers in a group's bitmaps' arithmetic hold.
const MOST_BLOCKS: u64 = 1 << 32;

const MAGIC: u16 = 0xEF53;
const EXTENT_MAGIC: u16 = 0xF30A;
const JBD2_MAGIC: u32 = 0xC03B_3998;
/// JBD2_SUPERBLOCK_V2.
const JBD2_SUPERBLOCK: u32 = 4;

/// Reserved inodes (ext4.h).
mod ino {
    pub const BAD: u32 = 1;
    pub const ROOT: u32 = 2;
    pub const JOURNAL: u32 = 8;
    /// The first an ordinary file may have (`s_first_ino`), which mke2fs gives lost+found.
    pub const LOST_FOUND: u32 = 11;
}

mod compat {
    pub const HAS_JOURNAL: u32 = 0x4;
    pub const EXT_ATTR: u32 = 0x8;
    pub const DIR_INDEX: u32 = 0x20;
}

mod incompat {
    pub const FILETYPE: u32 = 0x2;
    pub const EXTENTS: u32 = 0x40;
    pub const BIT64: u32 = 0x80;
    pub const FLEX_BG: u32 = 0x200;
}

mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x1;
    pub const LARGE_FILE: u32 = 0x2;
    pub const HUGE_FILE: u32 = 0x8;
    pub const DIR_NLINK: u32 = 0x20;
    pub const EXTRA_ISIZE: u32 = 0x40;
    pub const METADATA_CSUM: u32 = 0x400;
}

/// A group descriptor's `bg_flags`.
mod bg {
    pub const INODE_UNINIT: u16 = 0x1;
    pub const BLOCK_UNINIT: u16 = 0x2;
    pub const INODE_ZEROED: u16 = 0x4;
}

/// EXT4_EXTENTS_FL.
const EXTENTS_FL: u32 = 0x8_0000;
/// EXT4_FT_DIR.
const FT_DIR: u8 = 2;
/// The fake directory entry that holds a block's checksum: 12 bytes, file type 0xDE.
const DIR_TAIL: usize = 12;
const DIR_TAIL_FT: u8 = 0xDE;
/// lost+found's blocks, as mke2fs makes it: room for e2fsck to reconnect files without
/// growing it first (misc/mke2fs.c, create_lost_and_found).
const LOST_FOUND_BLOCKS: u64 = 4;

/// `s_flags`: directory hashes as an unsigned `char` makes them, as on arm64, whatever the
/// host (the kernel takes the flag as said).
const FLAGS_UNSIGNED_HASH: u32 = 0x2;
/// `s_default_mount_opts`: user xattrs and ACLs, mke2fs's.
const DEFAULT_MOUNT_OPTS: u32 = 0x000C;
/// `s_def_hash_version`: DX_HASH_HALF_MD4.
const HASH_HALF_MD4: u8 = 1;
/// `s_jnl_backup_type`: EXT3_JNL_BACKUP_BLOCKS.
const JNL_BACKUP_BLOCKS: u8 = 1;
/// `s_checksum_type`: EXT4_CRC32C_CHKSUM.
const CRC32C_CHKSUM: u8 = 1;
/// `s_errors`: EXT4_ERRORS_CONTINUE, mke2fs's default.
const ERRORS_CONTINUE: u16 = 1;

/// Where a file system's blocks are written: at byte offsets of the file it is made in.
pub trait Sink {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
}

impl Sink for std::fs::File {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::write_all_at(self, bytes, offset)
        }
        #[cfg(windows)]
        {
            let mut done = 0;
            while let Some(rest) = bytes.get(done..).filter(|r| !r.is_empty()) {
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
}

/// What a file system is made of: its size, its identity and its time.
#[derive(Debug, Clone)]
pub struct Options {
    /// Its size in bytes, whole blocks of [`BLOCK`]; a last group too small for its own
    /// metadata is left out, as mke2fs leaves it out.
    pub bytes: u64,
    pub uuid: [u8; 16],
    /// The seed of its directories' hashes (`s_hash_seed`).
    pub hash_seed: [u8; 16],
    /// When it was made, in seconds since the epoch.
    pub time: u64,
}

/// What was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub blocks: u64,
    pub groups: u64,
    pub inodes: u64,
    pub journal_blocks: u64,
    /// Blocks written: the rest of the file is a hole.
    pub written: u64,
}

/// Makes an empty ext4 file system of `options` in `sink`, from its byte 0: a root
/// directory holding lost+found, and a journal. Writes its metadata alone, every block
/// once; the file must be as long as the file system already, a hole where nothing is
/// written.
pub fn format(sink: &mut impl Sink, options: &Options) -> Result<Made, Error> {
    let g = Geometry::of(options.bytes)?;
    let csum_seed = crc32c(!0, &options.uuid);
    let mut written = 0u64;
    let mut put = |block: u64, data: &[u8]| -> Result<(), Error> {
        let at = block
            .checked_mul(BLOCK)
            .ok_or_else(|| Error(format!("block {block} is past any file")))?;
        sink.write_at(at, data)?;
        written += data.len().div_ceil(BLOCK as usize) as u64;
        Ok(())
    };
    // The inodes in use: the reserved ones, and lost+found.
    let used_inodes = u64::from(ino::LOST_FOUND);
    let descs = g.descriptors(csum_seed, used_inodes)?;
    let free_blocks: u64 = descs.iter().map(|d| d.free_blocks).sum();
    let superblock = |group: u64| g.superblock(options, free_blocks, used_inodes, group);
    // The superblock and descriptors, and every group's copies (sparse_super).
    let mut table =
        Buf::new(usize::try_from(g.gdt * BLOCK).map_err(|_| Error("a descriptor table past memory".into()))?);
    for (i, d) in descs.iter().enumerate() {
        table.put(i * DESC_SIZE as usize, &d.bytes)?;
    }
    for group in (0..g.groups).filter(|&group| g.has_super(group)) {
        let first = g.first(group);
        let mut block = Buf::new(BLOCK as usize);
        // The primary's at byte 1024 of block 0; a copy at its group's first byte.
        let at = if group == 0 { 1024 } else { 0 };
        block.put(at, &superblock(group)?)?;
        put(first, &block.0)?;
        put(first + 1, &table.0)?;
    }
    // The bitmaps of the groups whose blocks are not their own metadata alone, and the
    // first group's inodes': every other group's, uninitialized, the kernel works out.
    for (group, d) in descs.iter().enumerate() {
        if let Some(bitmap) = &d.block_bitmap {
            put(g.block_bitmap(group as u64), bitmap)?;
        }
        if let Some(bitmap) = &d.inode_bitmap {
            put(g.inode_bitmap(group as u64), bitmap)?;
        }
    }
    // The first inode table block: inodes 1 to 16.
    let mut inodes = Buf::new(BLOCK as usize);
    for ino in 1..=ino::LOST_FOUND {
        let inode = g.inode(options, csum_seed, ino)?;
        inodes.put((ino as usize - 1) * INODE_SIZE as usize, &inode)?;
    }
    put(g.inode_table(0), &inodes.0)?;
    // The root, and lost+found.
    put(
        g.root(),
        &dir_block(
            csum_seed,
            ino::ROOT,
            &[
                (b".", ino::ROOT),
                (b"..", ino::ROOT),
                (b"lost+found", ino::LOST_FOUND),
            ],
        )?,
    )?;
    put(
        g.lost_found(),
        &dir_block(
            csum_seed,
            ino::LOST_FOUND,
            &[(b".", ino::LOST_FOUND), (b"..", ino::ROOT)],
        )?,
    )?;
    for i in 1..LOST_FOUND_BLOCKS {
        put(g.lost_found() + i, &dir_block(csum_seed, ino::LOST_FOUND, &[])?)?;
    }
    // The journal's superblock: clean, as mke2fs writes it, its features the kernel's to
    // set as it mounts it (fs/ext4/super.c, set_journal_csum_feature_set).
    put(g.journal_start()?, &g.journal_superblock(options)?)?;
    Ok(Made {
        blocks: g.blocks,
        groups: g.groups,
        inodes: g.groups * g.ipg,
        journal_blocks: g.journal,
        written,
    })
}

/// The size of the least file system, in whole `align`-byte steps, whose blocks free once
/// made ([`format`]) hold `free` bytes for its files: what a writable layer asked to hold
/// that much is made of, its own metadata and journal beside it, and the blocks the kernel
/// keeps back from files as it mounts it, 2% or 4096 clusters, the fewer
/// (super.c, ext4_calculate_resv_clusters).
pub fn size_for(free: u64, align: u64) -> Result<u64, Error> {
    if align == 0 || !align.is_multiple_of(BLOCK) {
        return err(format!("a file system in steps of {align} bytes"));
    }
    let too_large = || {
        Error(format!(
            "an ext4 file system holding {free} bytes is past its most"
        ))
    };
    let mut bytes = free
        .max(LEAST_BLOCKS * BLOCK)
        .checked_next_multiple_of(align)
        .ok_or_else(too_large)?;
    loop {
        let g = Geometry::of(bytes)?;
        let kept_back = (g.blocks / 50).min(4096);
        let held: u64 = g
            .descriptors(0, u64::from(ino::LOST_FOUND))?
            .iter()
            .map(|d| d.free_blocks)
            .sum::<u64>()
            .saturating_sub(kept_back)
            * BLOCK;
        if held >= free && g.blocks * BLOCK == bytes {
            return Ok(bytes);
        }
        // Short by what its metadata takes, or by a last group left out: that much more.
        let short = free.saturating_sub(held).max(bytes - g.blocks * BLOCK).max(align);
        bytes = bytes
            .checked_add(short)
            .and_then(|b| b.checked_next_multiple_of(align))
            .ok_or_else(too_large)?;
    }
}

/// A buffer whose fields are written at offsets, each checked to lie within it.
struct Buf(Vec<u8>);

impl Buf {
    fn new(len: usize) -> Buf {
        Buf(vec![0; len])
    }

    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<(), Error> {
        let len = self.0.len();
        at.checked_add(bytes.len())
            .and_then(|end| self.0.get_mut(at..end))
            .ok_or_else(|| Error(format!("{} bytes at {at} of {len}", bytes.len())))?
            .copy_from_slice(bytes);
        Ok(())
    }

    fn u8(&mut self, at: usize, v: u8) -> Result<(), Error> {
        self.put(at, &[v])
    }

    fn u16(&mut self, at: usize, v: u16) -> Result<(), Error> {
        self.put(at, &v.to_le_bytes())
    }

    fn u32(&mut self, at: usize, v: u32) -> Result<(), Error> {
        self.put(at, &v.to_le_bytes())
    }

    /// The low 32 bits of `v` at `lo`, the next 32 at `hi`.
    fn split(&mut self, lo: usize, hi: usize, v: u64) -> Result<(), Error> {
        self.u32(lo, v as u32)?;
        self.u32(hi, (v >> 32) as u32)
    }

    fn get(&self, range: std::ops::Range<usize>) -> Result<&[u8], Error> {
        let len = self.0.len();
        self.0
            .get(range.clone())
            .ok_or_else(|| Error(format!("bytes {range:?} of {len}")))
    }
}

/// A file system's shape: its groups, inodes, tables and journal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Geometry {
    blocks: u64,
    groups: u64,
    /// Inodes per group.
    ipg: u64,
    /// Inode table blocks per group.
    itb: u64,
    /// Group descriptor table blocks.
    gdt: u64,
    /// Journal blocks.
    journal: u64,
}

/// A group's descriptor, and the bitmaps written for it.
struct Descriptor {
    bytes: [u8; DESC_SIZE as usize],
    free_blocks: u64,
    block_bitmap: Option<Vec<u8>>,
    inode_bitmap: Option<Vec<u8>>,
}

impl Geometry {
    fn of(bytes: u64) -> Result<Geometry, Error> {
        if !bytes.is_multiple_of(BLOCK) {
            return err(format!("{bytes} bytes are not whole {BLOCK}-byte blocks"));
        }
        let mut blocks = bytes / BLOCK;
        if blocks < LEAST_BLOCKS {
            return err(format!(
                "an ext4 file system of {bytes} bytes: at least {} MiB",
                (LEAST_BLOCKS * BLOCK) >> 20
            ));
        }
        if blocks > MOST_BLOCKS {
            return err(format!(
                "an ext4 file system of {bytes} bytes: at most {} TiB",
                (MOST_BLOCKS * BLOCK) >> 40
            ));
        }
        loop {
            let groups = blocks.div_ceil(GROUP);
            let inodes = (blocks * BLOCK / INODE_RATIO).max(1);
            // Whole inode table blocks a group.
            let per_block = BLOCK / INODE_SIZE;
            let ipg = inodes.div_ceil(groups).next_multiple_of(per_block).min(GROUP);
            let g = Geometry {
                blocks,
                groups,
                ipg,
                itb: ipg * INODE_SIZE / BLOCK,
                gdt: (groups * DESC_SIZE).div_ceil(BLOCK),
                journal: journal_blocks(blocks),
            };
            // A last group too small for what lies in it, and a block of its own, is left
            // out (mke2fs: lib/ext2fs/initialize.c, "overhead").
            let last = groups - 1;
            if last > 0 && g.size(last) < g.overhead(last) + 1 {
                blocks = g.first(last);
                continue;
            }
            if g.root() + 1 + LOST_FOUND_BLOCKS > g.size(0) {
                return err(format!("an ext4 file system of {bytes} bytes holds no root"));
            }
            g.journal_start()?;
            return Ok(g);
        }
    }

    fn first(&self, group: u64) -> u64 {
        group * GROUP
    }

    fn size(&self, group: u64) -> u64 {
        (self.blocks - self.first(group)).min(GROUP)
    }

    /// Whether `group` holds a copy of the superblock and descriptors (sparse_super): the
    /// first two, and the powers of 3, 5 and 7 (ext4_bg_has_super).
    fn has_super(&self, group: u64) -> bool {
        let power = |mut n: u64, base: u64| {
            while n > 1 && n.is_multiple_of(base) {
                n /= base;
            }
            n == 1
        };
        group <= 1 || (group % 2 == 1 && (power(group, 3) || power(group, 5) || power(group, 7)))
    }

    /// The superblock and descriptor blocks at `group`'s start, where it holds them.
    fn super_len(&self, group: u64) -> u64 {
        if self.has_super(group) { 1 + self.gdt } else { 0 }
    }

    /// The groups of `group`'s flex group whose bitmaps and tables lie in `group`, where it
    /// leads one; none otherwise.
    fn members(&self, group: u64) -> u64 {
        if group.is_multiple_of(FLEX) {
            (self.groups - group).min(FLEX)
        } else {
            0
        }
    }

    /// What lies in `group` before any file's blocks: its superblock copy and the bitmaps
    /// and tables of the groups it leads.
    fn overhead(&self, group: u64) -> u64 {
        self.super_len(group) + self.members(group) * (2 + self.itb)
    }

    fn leader(&self, group: u64) -> u64 {
        group / FLEX * FLEX
    }

    fn meta_start(&self, group: u64) -> u64 {
        let leader = self.leader(group);
        self.first(leader) + self.super_len(leader)
    }

    fn block_bitmap(&self, group: u64) -> u64 {
        self.meta_start(group) + (group - self.leader(group))
    }

    fn inode_bitmap(&self, group: u64) -> u64 {
        let leader = self.leader(group);
        self.meta_start(group) + self.members(leader) + (group - leader)
    }

    fn inode_table(&self, group: u64) -> u64 {
        let leader = self.leader(group);
        self.meta_start(group) + 2 * self.members(leader) + (group - leader) * self.itb
    }

    /// The root directory's block: the first after the first flex group's tables.
    fn root(&self) -> u64 {
        self.overhead(0)
    }

    fn lost_found(&self) -> u64 {
        self.root() + 1
    }

    /// The journal's first block: group 2's, which holds neither a superblock copy nor a
    /// flex group's tables, where it fits there (mke2fs puts its own toward the middle);
    /// else after lost+found in group 0.
    fn journal_start(&self) -> Result<u64, Error> {
        if self.groups > 2 && self.size(2) >= self.journal {
            return Ok(self.first(2));
        }
        let after = self.lost_found() + LOST_FOUND_BLOCKS;
        if after + self.journal <= self.size(0) {
            return Ok(after);
        }
        err(format!(
            "an ext4 file system of {} blocks has no room for its {}-block journal",
            self.blocks, self.journal
        ))
    }

    /// The blocks of `group` in use, as ranges of block numbers.
    fn used(&self, group: u64) -> Result<Vec<std::ops::Range<u64>>, Error> {
        let first = self.first(group);
        let mut used = Vec::with_capacity(3);
        used.push(first..first + self.overhead(group));
        if group == 0 {
            used.push(self.root()..self.lost_found() + LOST_FOUND_BLOCKS);
        }
        let journal = self.journal_start()?;
        if self.group_of(journal) == group {
            used.push(journal..journal + self.journal);
        }
        Ok(used)
    }

    fn group_of(&self, block: u64) -> u64 {
        block / GROUP
    }

    /// Every group's descriptor, and the bitmaps written with it: the inodes in use are
    /// the first `used_inodes`, all in group 0.
    fn descriptors(&self, csum_seed: u32, used_inodes: u64) -> Result<Vec<Descriptor>, Error> {
        let journal_group = self.group_of(self.journal_start()?);
        (0..self.groups)
            .map(|group| {
                let used = self.used(group)?;
                let in_use: u64 = used.iter().map(|r| r.end - r.start).sum();
                let free_blocks = self.size(group).checked_sub(in_use).ok_or_else(|| {
                    Error(format!(
                        "group {group}: {in_use} blocks in use of {}",
                        self.size(group)
                    ))
                })?;
                // Uninitialized, the kernel makes a group's block bitmap of its superblock
                // copy and its own bitmaps and table alone (balloc.c,
                // ext4_init_block_bitmap): so not a group holding other groups' tables, or
                // files; nor the last, which e2fsck wants written.
                let block_init = group == 0
                    || self.members(group) > 0
                    || group == journal_group
                    || group == self.groups - 1;
                let block_bitmap = block_init
                    .then(|| self.block_bitmap_of(group, &used))
                    .transpose()?;
                let inodes_here = if group == 0 { used_inodes } else { 0 };
                let inode_bitmap = (group == 0)
                    .then(|| self.inode_bitmap_of(inodes_here))
                    .transpose()?;
                let mut d = Buf::new(DESC_SIZE as usize);
                d.split(0x00, 0x20, self.block_bitmap(group))?;
                d.split(0x04, 0x24, self.inode_bitmap(group))?;
                d.split(0x08, 0x28, self.inode_table(group))?;
                let free_inodes = self.ipg - inodes_here;
                d.u16(0x0C, free_blocks as u16)?;
                d.u16(0x2C, (free_blocks >> 16) as u16)?;
                d.u16(0x0E, free_inodes as u16)?;
                d.u16(0x2E, (free_inodes >> 16) as u16)?;
                let dirs: u64 = if group == 0 { 2 } else { 0 };
                d.u16(0x10, dirs as u16)?;
                let mut flags = bg::INODE_ZEROED;
                if group != 0 {
                    flags |= bg::INODE_UNINIT;
                }
                if !block_init {
                    flags |= bg::BLOCK_UNINIT;
                }
                d.u16(0x12, flags)?;
                if let Some(bitmap) = &block_bitmap {
                    let csum = crc32c(csum_seed, bitmap.get(..(GROUP / 8) as usize).unwrap_or_default());
                    d.u16(0x18, csum as u16)?;
                    d.u16(0x38, (csum >> 16) as u16)?;
                }
                if let Some(bitmap) = &inode_bitmap {
                    let csum = crc32c(
                        csum_seed,
                        bitmap.get(..(self.ipg / 8) as usize).unwrap_or_default(),
                    );
                    d.u16(0x1A, csum as u16)?;
                    d.u16(0x3A, (csum >> 16) as u16)?;
                }
                // The inodes past the last in use, which no scan need read.
                let unused = self.ipg - inodes_here;
                d.u16(0x1C, unused as u16)?;
                d.u16(0x32, (unused >> 16) as u16)?;
                let csum = group_desc_csum(csum_seed, group, &d.0)?;
                d.u16(0x1E, csum)?;
                let mut bytes = [0; DESC_SIZE as usize];
                bytes.copy_from_slice(d.get(0..DESC_SIZE as usize)?);
                Ok(Descriptor {
                    bytes,
                    free_blocks,
                    block_bitmap,
                    inode_bitmap,
                })
            })
            .collect()
    }

    /// `group`'s block bitmap: its `used` blocks, and the bits past its end set
    /// (ext4_mark_bitmap_end).
    fn block_bitmap_of(&self, group: u64, used: &[std::ops::Range<u64>]) -> Result<Vec<u8>, Error> {
        let first = self.first(group);
        let mut bits = Bitmap::new();
        for range in used {
            for block in range.clone() {
                bits.set(block - first)?;
            }
        }
        for bit in self.size(group)..GROUP {
            bits.set(bit)?;
        }
        Ok(bits.0)
    }

    /// An inode bitmap with its first `used` inodes in use, and the bits past the group's
    /// inodes set.
    fn inode_bitmap_of(&self, used: u64) -> Result<Vec<u8>, Error> {
        let mut bits = Bitmap::new();
        for bit in (0..used).chain(self.ipg..GROUP) {
            bits.set(bit)?;
        }
        Ok(bits.0)
    }

    /// The superblock, as group `group` holds it (`s_block_group_nr`).
    fn superblock(
        &self,
        o: &Options,
        free_blocks: u64,
        used_inodes: u64,
        group: u64,
    ) -> Result<Vec<u8>, Error> {
        let mut s = Buf::new(1024);
        let inodes = self.groups * self.ipg;
        let narrow = |v: u64, what: &str| u32::try_from(v).map_err(|_| Error(format!("{v} {what}")));
        s.u32(0x00, narrow(inodes, "inodes")?)?;
        s.split(0x04, 0x150, self.blocks)?;
        s.split(0x0C, 0x158, free_blocks)?;
        s.u32(0x10, narrow(inodes - used_inodes, "free inodes")?)?;
        s.u32(0x18, LOG_BLOCK)?;
        s.u32(0x1C, LOG_BLOCK)?;
        s.u32(0x20, GROUP as u32)?;
        s.u32(0x24, GROUP as u32)?;
        s.u32(0x28, narrow(self.ipg, "inodes a group")?)?;
        let (time, time_hi) = (o.time as u32, (o.time >> 32) as u8);
        s.u32(0x30, time)?; // s_wtime
        s.u16(0x36, u16::MAX)?; // s_max_mnt_count: no check by count
        s.u16(0x38, MAGIC)?;
        s.u16(0x3A, 1)?; // s_state: EXT4_VALID_FS
        s.u16(0x3C, ERRORS_CONTINUE)?;
        s.u32(0x40, time)?; // s_lastcheck
        s.u32(0x4C, 1)?; // s_rev_level: EXT4_DYNAMIC_REV
        s.u32(0x54, ino::LOST_FOUND)?; // s_first_ino
        s.u16(0x58, INODE_SIZE as u16)?;
        s.u16(0x5A, narrow(group, "groups")? as u16)?;
        s.u32(0x5C, compat::HAS_JOURNAL | compat::EXT_ATTR | compat::DIR_INDEX)?;
        s.u32(
            0x60,
            incompat::FILETYPE | incompat::EXTENTS | incompat::BIT64 | incompat::FLEX_BG,
        )?;
        s.u32(
            0x64,
            ro_compat::SPARSE_SUPER
                | ro_compat::LARGE_FILE
                | ro_compat::HUGE_FILE
                | ro_compat::DIR_NLINK
                | ro_compat::EXTRA_ISIZE
                | ro_compat::METADATA_CSUM,
        )?;
        s.put(0x68, &o.uuid)?;
        s.u32(0xE0, ino::JOURNAL)?;
        s.put(0xEC, &o.hash_seed)?;
        s.u8(0xFC, HASH_HALF_MD4)?;
        s.u8(0xFD, JNL_BACKUP_BLOCKS)?;
        s.u16(0xFE, DESC_SIZE as u16)?;
        s.u32(0x100, DEFAULT_MOUNT_OPTS)?;
        s.u32(0x108, time)?; // s_mkfs_time
        // The journal inode's blocks and size, kept against its loss (s_jnl_blocks).
        let journal = self.journal_inode_blocks()?;
        s.put(0x10C, &journal)?;
        s.u32(0x10C + 15 * 4, ((self.journal * BLOCK) >> 32) as u32)?;
        s.u32(0x10C + 16 * 4, (self.journal * BLOCK) as u32)?;
        s.u16(0x15C, EXTRA_ISIZE)?;
        s.u16(0x15E, EXTRA_ISIZE)?;
        s.u32(0x160, FLAGS_UNSIGNED_HASH)?;
        s.u8(0x174, LOG_FLEX as u8)?;
        s.u8(0x175, CRC32C_CHKSUM)?;
        for at in [0x274, 0x276, 0x277] {
            s.u8(at, time_hi)?; // s_wtime_hi, s_mkfs_time_hi, s_lastcheck_hi
        }
        let csum = crc32c(!0, s.get(0..0x3FC)?);
        s.u32(0x3FC, csum)?;
        Ok(s.0)
    }

    /// The journal inode's `i_block`: one extent over the whole journal.
    fn journal_inode_blocks(&self) -> Result<[u8; 60], Error> {
        extents(&[(self.journal_start()?, self.journal)])
    }

    /// Inode `ino` of the reserved ones and lost+found, as mke2fs writes them: the root
    /// and lost+found directories, the journal, the bad blocks inode with its times, and
    /// the rest empty, each with its checksum.
    fn inode(&self, o: &Options, csum_seed: u32, ino: u32) -> Result<Vec<u8>, Error> {
        let mut i = Buf::new(INODE_SIZE as usize);
        let (time, time_extra) = (o.time as u32, ((o.time >> 32) & 3) as u32);
        let file =
            |i: &mut Buf, mode: u16, links: u16, size: u64, blocks: &[(u64, u64)]| -> Result<(), Error> {
                i.u16(0x00, mode)?;
                i.split(0x04, 0x6C, size)?;
                for at in [0x08, 0x0C, 0x10, 0x90] {
                    i.u32(at, time)?; // atime, ctime, mtime, crtime
                }
                for at in [0x84, 0x88, 0x8C, 0x94] {
                    i.u32(at, time_extra)?; // their epoch bits
                }
                i.u16(0x1A, links)?;
                let sectors = blocks.iter().map(|&(_, n)| n).sum::<u64>() * (BLOCK / 512);
                i.u32(0x1C, sectors as u32)?;
                i.u16(0x74, (sectors >> 32) as u16)?;
                i.u32(0x20, EXTENTS_FL)?;
                i.put(0x28, &extents(blocks)?)?;
                i.u16(0x80, EXTRA_ISIZE)
            };
        match ino {
            ino::ROOT => file(&mut i, 0o040_755, 3, BLOCK, &[(self.root(), 1)])?,
            ino::LOST_FOUND => file(
                &mut i,
                0o040_700,
                2,
                LOST_FOUND_BLOCKS * BLOCK,
                &[(self.lost_found(), LOST_FOUND_BLOCKS)],
            )?,
            ino::JOURNAL => file(
                &mut i,
                0o100_600,
                1,
                self.journal * BLOCK,
                &[(self.journal_start()?, self.journal)],
            )?,
            ino::BAD => {
                for at in [0x08, 0x0C, 0x10] {
                    i.u32(at, time)?;
                }
            }
            _ => {}
        }
        let csum = inode_csum(csum_seed, ino, &i.0)?;
        i.u16(0x7C, csum as u16)?;
        // The high half where the inode's extra fields reach it (EXT4_FITS_IN_INODE).
        if ino == ino::ROOT || ino == ino::LOST_FOUND || ino == ino::JOURNAL {
            i.u16(0x82, (csum >> 16) as u16)?;
        }
        Ok(i.0)
    }

    /// The journal's superblock (include/linux/jbd2.h, journal_superblock_t), big-endian:
    /// version 2, empty (`s_start` 0), its first log block 1, its sequence 1, one user,
    /// the file system's UUID.
    fn journal_superblock(&self, o: &Options) -> Result<Vec<u8>, Error> {
        let mut j = Buf::new(BLOCK as usize);
        let be = |j: &mut Buf, at: usize, v: u32| j.put(at, &v.to_be_bytes());
        be(&mut j, 0x00, JBD2_MAGIC)?;
        be(&mut j, 0x04, JBD2_SUPERBLOCK)?;
        be(&mut j, 0x0C, BLOCK as u32)?;
        be(
            &mut j,
            0x10,
            u32::try_from(self.journal).map_err(|_| Error("a journal past 32-bit block counts".into()))?,
        )?;
        be(&mut j, 0x14, 1)?; // s_first
        be(&mut j, 0x18, 1)?; // s_sequence
        j.put(0x30, &o.uuid)?;
        be(&mut j, 0x40, 1)?; // s_nr_users
        Ok(j.0)
    }
}

/// The journal's size in blocks, as mke2fs sizes one for a file system of `blocks`
/// (lib/ext2fs/mkjournal.c, ext2fs_default_journal_size), at most a group: a group holds
/// it whole, and a layer's transactions, at most a quarter of it (jbd2's
/// j_max_transaction_buffers), have 32 MiB.
fn journal_blocks(blocks: u64) -> u64 {
    let mke2fs = match blocks {
        b if b < 32 << 10 => 1024,
        b if b < 256 << 10 => 4096,
        b if b < 512 << 10 => 8192,
        b if b < 4096 << 10 => 16384,
        b if b < 8192 << 10 => 32768,
        b if b < 16384 << 10 => 65536,
        b if b < 32768 << 10 => 131_072,
        _ => 262_144,
    };
    mke2fs.min(GROUP)
}

/// An inode's `i_block` holding `runs`, `(first block, count)`, as extents in its own
/// header: at most four, each at most 32768 blocks (EXT_INIT_MAX_LEN).
fn extents(runs: &[(u64, u64)]) -> Result<[u8; 60], Error> {
    const MAX_LEN: u64 = 32768;
    if runs.len() > 4 || runs.iter().any(|&(_, n)| n == 0 || n > MAX_LEN) {
        return err(format!("extents {runs:?} do not fit in an inode"));
    }
    let mut b = Buf::new(60);
    b.u16(0, EXTENT_MAGIC)?;
    b.u16(2, runs.len() as u16)?;
    b.u16(4, 4)?; // eh_max
    let mut logical = 0u64;
    for (i, &(start, count)) in runs.iter().enumerate() {
        let at = 12 + 12 * i;
        b.u32(
            at,
            u32::try_from(logical).map_err(|_| Error("a file past 2^32 blocks".into()))?,
        )?;
        b.u16(at + 4, count as u16)?;
        b.u16(at + 6, (start >> 32) as u16)?;
        b.u32(at + 8, start as u32)?;
        logical += count;
    }
    let mut out = [0; 60];
    out.copy_from_slice(b.get(0..60)?);
    Ok(out)
}

/// A directory block of `entries` (name, inode), directories all, the last reaching to
/// its checksum's tail (ext4_dir_entry_tail); none, one empty entry over the block.
fn dir_block(csum_seed: u32, dir: u32, entries: &[(&[u8], u32)]) -> Result<Vec<u8>, Error> {
    let mut b = Buf::new(BLOCK as usize);
    let end = BLOCK as usize - DIR_TAIL;
    let mut at = 0;
    if entries.is_empty() {
        b.u16(4, end as u16)?;
    }
    for (i, &(name, inode)) in entries.iter().enumerate() {
        let len = (8 + name.len()).next_multiple_of(4);
        let rec_len = if i + 1 == entries.len() { end - at } else { len };
        b.u32(at, inode)?;
        b.u16(at + 4, rec_len as u16)?;
        b.u8(at + 6, name.len() as u8)?;
        b.u8(at + 7, FT_DIR)?;
        b.put(at + 8, name)?;
        at += rec_len;
    }
    // The tail: an entry of no inode, 12 bytes long, file type 0xDE, then the checksum.
    b.u16(end + 4, DIR_TAIL as u16)?;
    b.u8(end + 7, DIR_TAIL_FT)?;
    let csum = crc32c(inode_seed(csum_seed, dir, 0), b.get(0..end)?);
    b.u32(end + 8, csum)?;
    Ok(b.0)
}

/// A block's worth of bitmap.
struct Bitmap(Vec<u8>);

impl Bitmap {
    fn new() -> Bitmap {
        Bitmap(vec![0; BLOCK as usize])
    }

    fn set(&mut self, bit: u64) -> Result<(), Error> {
        let byte = self
            .0
            .get_mut((bit / 8) as usize)
            .ok_or_else(|| Error(format!("bit {bit} past a bitmap block")))?;
        *byte |= 1 << (bit % 8);
        Ok(())
    }
}

/// An inode's checksum seed: its number, then its generation, over the file system's
/// (ext4_iget).
fn inode_seed(csum_seed: u32, ino: u32, generation: u32) -> u32 {
    crc32c(crc32c(csum_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
}

/// An inode's checksum (inode.c, ext4_inode_csum): over the whole inode, its checksum's
/// halves taken as zeros, the high half where the inode's extra size reaches it.
fn inode_csum(csum_seed: u32, ino: u32, raw: &[u8]) -> Result<u32, Error> {
    let get = |r: std::ops::Range<usize>| {
        raw.get(r.clone())
            .ok_or_else(|| Error(format!("inode bytes {r:?} of {}", raw.len())))
    };
    let generation = u32::from_le_bytes(
        get(0x64..0x68)?
            .try_into()
            .map_err(|_| Error("i_generation".into()))?,
    );
    let extra = u16::from_le_bytes(
        get(0x80..0x82)?
            .try_into()
            .map_err(|_| Error("i_extra_isize".into()))?,
    );
    let mut csum = inode_seed(csum_seed, ino, generation);
    csum = crc32c(csum, get(0..0x7C)?);
    csum = crc32c(csum, &[0, 0]);
    csum = crc32c(csum, get(0x7E..GOOD_OLD_INODE_SIZE)?);
    csum = crc32c(csum, get(GOOD_OLD_INODE_SIZE..0x82)?);
    let hi_fits = 0x84 <= GOOD_OLD_INODE_SIZE + usize::from(extra);
    let rest = if hi_fits {
        csum = crc32c(csum, &[0, 0]);
        0x84
    } else {
        0x82
    };
    Ok(crc32c(csum, get(rest..raw.len())?))
}

/// A group descriptor's checksum (super.c, ext4_group_desc_csum): over its group number
/// and the descriptor, its checksum taken as zero; the low 16 bits.
fn group_desc_csum(csum_seed: u32, group: u64, desc: &[u8]) -> Result<u16, Error> {
    let group = u32::try_from(group).map_err(|_| Error(format!("group {group}")))?;
    let mut csum = crc32c(csum_seed, &group.to_le_bytes());
    csum = crc32c(
        csum,
        desc.get(..0x1E)
            .ok_or_else(|| Error("a short descriptor".into()))?,
    );
    csum = crc32c(csum, &[0, 0]);
    csum = crc32c(
        csum,
        desc.get(0x20..)
            .ok_or_else(|| Error("a short descriptor".into()))?,
    );
    Ok(csum as u16)
}

/// CRC-32C (Castagnoli, reflected polynomial 0x82F63B78) of `data` from `crc`, with no
/// inversion before or after: Linux's `crc32c()`, which ext4 and jbd2 seed with `~0`.
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    data.iter().fold(crc, |crc, &b| {
        let i = ((crc ^ u32::from(b)) & 0xFF) as usize;
        CRC32C.get(i).copied().unwrap_or_default() ^ (crc >> 8)
    })
}

/// The Castagnoli table.
const CRC32C: [u32; 256] = crc32c_table();

// Evaluated at compile time only: an index out of range fails the build, never a run.
#[allow(clippy::indexing_slicing)]
const fn crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests;
