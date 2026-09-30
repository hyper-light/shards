//! Writes EROFS images as Linux 6.18 reads them (fs/erofs/erofs_fs.h, inode.c, data.c,
//! dir.c, namei.c and xattr.c at v6.18.48): uncompressed, 4 KiB blocks, inline file tails,
//! inline xattrs, and no feature flags. The bytes are a function of the tree alone.
//!
//! Layout. Block 0 holds the superblock at byte 1024. The metadata area starts at block 0
//! (`meta_blkaddr` 0), so inode numbers (nid = byte offset / 32) start after the
//! superblock, at 36, as erofs-utils' do, and never 0: glibc's readdir skips entries whose
//! inode number is 0. Every inode record (inode, inline xattrs, inline tail) stays within
//! one block, as the kernel requires of inline data (data.c). Data blocks follow the
//! metadata, in inode order.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};

use crate::{Error, bad as err};

pub const BLOCK_BITS: u8 = 12;
pub const BLOCK: u64 = 1 << BLOCK_BITS;
const SUPER_OFFSET: u64 = 1024;
const SUPER_SIZE: u64 = 128;
const MAGIC: u32 = 0xE0F5_E1E2;
/// Inodes sit in 32-byte slots; a nid is the slot index from the metadata area's start.
const SLOT: u64 = 32;
const COMPACT: u64 = 32;
const EXTENDED: u64 = 64;
const DIRENT: u64 = 12;
const XATTR_HEADER: u64 = 12;
const NAME_MAX: usize = 255;
const FLAT_PLAIN: u16 = 0;
const FLAT_INLINE: u16 = 2;
/// EROFS_NULL_ADDR: a file with no data blocks.
const NULL_ADDR: u32 = u32::MAX;

/// File types in directory entries (the kernel's FT_* values).
mod ft {
    pub const REG: u8 = 1;
    pub const DIR: u8 = 2;
    pub const CHR: u8 = 3;
    pub const BLK: u8 = 4;
    pub const FIFO: u8 = 5;
    pub const SOCK: u8 = 6;
    pub const LNK: u8 = 7;
}

/// File type bits of `i_mode` (S_IF*).
mod ifmt {
    pub const SOCK: u16 = 0o140_000;
    pub const LNK: u16 = 0o120_000;
    pub const REG: u16 = 0o100_000;
    pub const BLK: u16 = 0o060_000;
    pub const DIR: u16 = 0o040_000;
    pub const CHR: u16 = 0o020_000;
    pub const FIFO: u16 = 0o010_000;
}

pub type NodeId = usize;

/// A directory entry: its name, borrowed from the tree, and what it names.
type Entry<'a> = (&'a [u8], NodeId);

/// Zeros to pad with: no pad is longer than a block.
static ZEROS: [u8; BLOCK as usize] = [0; BLOCK as usize];

/// Ownership, permissions, times and extended attributes of a node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// Permission and set-id bits (0o7777); the file type comes from the node's kind.
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub mtime_nsec: u32,
    /// Full names, such as `user.foo` or `security.capability`.
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// Where a regular file's bytes are, for the [`Source`] that reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRef {
    pub source: u32,
    pub offset: u64,
}

/// Reads file contents while the image is written.
pub trait Source {
    /// Fills `buf` with the file's bytes from `at`.
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir(BTreeMap<Vec<u8>, NodeId>),
    File { size: u64, data: DataRef },
    Symlink(Vec<u8>),
    CharDevice { major: u32, minor: u32 },
    BlockDevice { major: u32, minor: u32 },
    Fifo,
    Socket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: Kind,
    pub meta: Meta,
}

/// A directory tree to write. Nodes removed from it stay in the arena but are not
/// written: only what the root reaches is.
#[derive(Debug, Clone)]
pub struct Tree {
    nodes: Vec<Node>,
}

fn check_name(name: &[u8]) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > NAME_MAX
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return err(format!(
            "{:?} is not a valid file name",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(())
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn new(root: Meta) -> Tree {
        Tree {
            nodes: vec![Node {
                kind: Kind::Dir(BTreeMap::new()),
                meta: root,
            }],
        }
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id)
    }

    fn entries_mut(&mut self, dir: NodeId) -> Result<&mut BTreeMap<Vec<u8>, NodeId>, Error> {
        match self.nodes.get_mut(dir).map(|n| &mut n.kind) {
            Some(Kind::Dir(entries)) => Ok(entries),
            _ => err("not a directory"),
        }
    }

    /// The entry `name` in directory `dir`.
    pub fn child(&self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        match self.nodes.get(dir).map(|n| &n.kind) {
            Some(Kind::Dir(entries)) => entries.get(name).copied(),
            _ => None,
        }
    }

    /// Adds `node` to `dir` as `name`, replacing any entry of that name.
    pub fn insert(&mut self, dir: NodeId, name: &[u8], node: Node) -> Result<NodeId, Error> {
        check_name(name)?;
        let id = self.nodes.len();
        self.entries_mut(dir)?;
        self.nodes.push(node);
        self.entries_mut(dir)?.insert(name.to_vec(), id);
        Ok(id)
    }

    /// Gives `target`, which is not a directory, another name: a hard link.
    pub fn link(&mut self, dir: NodeId, name: &[u8], target: NodeId) -> Result<(), Error> {
        check_name(name)?;
        match self.nodes.get(target).map(|n| &n.kind) {
            None => return err("hard link to a missing node"),
            Some(Kind::Dir(_)) => return err("hard link to a directory"),
            Some(_) => {}
        }
        self.entries_mut(dir)?.insert(name.to_vec(), target);
        Ok(())
    }

    /// Removes the entry `name` from `dir`, returning what it named.
    pub fn remove(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.entries_mut(dir).ok()?.remove(name)
    }

    /// Removes every entry of `dir`.
    pub fn clear(&mut self, dir: NodeId) {
        if let Ok(entries) = self.entries_mut(dir) {
            entries.clear();
        }
    }
}

/// What was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub blocks: u64,
    pub inodes: u64,
}

/// An xattr name as EROFS stores it: a prefix index and the rest of the name.
fn xattr_index(name: &[u8]) -> Option<(u8, &[u8])> {
    const PREFIXES: [(&[u8], u8); 6] = [
        (b"user.", 1),
        (b"system.posix_acl_access", 2),
        (b"system.posix_acl_default", 3),
        (b"trusted.", 4),
        (b"lustre.", 5),
        (b"security.", 6),
    ];
    PREFIXES
        .iter()
        .find_map(|(p, i)| name.strip_prefix(*p).map(|rest| (*i, rest)))
}

/// The inline xattr body into `body`: header, then entries in (index, name) order, each
/// padded to 4. Its length is [`xattr_len`]'s.
fn xattr_body(xattrs: &BTreeMap<Vec<u8>, Vec<u8>>, body: &mut Vec<u8>) -> Result<(), Error> {
    body.clear();
    if xattrs.is_empty() {
        return Ok(());
    }
    let mut entries = Vec::with_capacity(xattrs.len());
    for (name, value) in xattrs {
        let Some((index, suffix)) = xattr_index(name) else {
            return err(format!(
                "xattr {:?} has a namespace EROFS cannot store",
                String::from_utf8_lossy(name)
            ));
        };
        let name_len = u8::try_from(suffix.len()).map_err(|_| Error("xattr name too long".into()))?;
        let value_len = u16::try_from(value.len()).map_err(|_| Error("xattr value too long".into()))?;
        entries.push((index, suffix, name_len, value_len, value));
    }
    entries.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    body.resize(XATTR_HEADER as usize, 0);
    for (index, suffix, name_len, value_len, value) in entries {
        body.push(name_len);
        body.push(index);
        body.extend_from_slice(&value_len.to_le_bytes());
        body.extend_from_slice(suffix);
        body.extend_from_slice(value);
        body.resize(body.len().next_multiple_of(4), 0);
    }
    Ok(())
}

/// The length of [`xattr_body`]'s body, which the layout needs before any is written.
fn xattr_len(xattrs: &BTreeMap<Vec<u8>, Vec<u8>>) -> u64 {
    if xattrs.is_empty() {
        return 0;
    }
    xattrs.iter().fold(XATTR_HEADER, |n, (name, value)| {
        let suffix = xattr_index(name).map_or(name.len(), |(_, rest)| rest.len());
        n + (4 + suffix + value.len()).next_multiple_of(4) as u64
    })
}

/// One inode as laid out: everything the writer decides before writing.
#[derive(Debug)]
struct Inode<'a> {
    node: NodeId,
    nlink: u32,
    /// For directories: the parent's node.
    parent: NodeId,
    extended: bool,
    /// The length of its xattr body, which is built as its record is written.
    xattrs: u64,
    size: u64,
    /// Bytes of data kept in the inode record (FLAT_INLINE), or 0.
    tail: u64,
    inline: bool,
    nid: u64,
    /// First data block, if the inode has any.
    start: Option<u64>,
    /// A directory's entries, sorted, and where each of its blocks ends among them.
    entries: Vec<Entry<'a>>,
    block_ends: Vec<usize>,
}

impl Inode<'_> {
    fn isize(&self) -> u64 {
        if self.extended { EXTENDED } else { COMPACT }
    }

    fn data_blocks(&self) -> u64 {
        if self.inline {
            self.size / BLOCK
        } else {
            self.size.div_ceil(BLOCK)
        }
    }

    /// A directory's blocks, as runs of its entries.
    fn dir_blocks(&self) -> impl Iterator<Item = &[Entry<'_>]> {
        let starts = std::iter::once(0).chain(self.block_ends.iter().copied());
        starts
            .zip(self.block_ends.iter().copied())
            .map(|(from, to)| self.entries.get(from..to).unwrap_or_default())
    }
}

/// Where sorted entries split into directory blocks, each holding 12-byte dirents, then
/// names, within one block: each block's end among them, and the directory's size.
fn dir_blocks(entries: &[Entry<'_>]) -> (Vec<usize>, u64) {
    let mut ends = Vec::new();
    let mut used = 0u64;
    for (i, (name, _)) in entries.iter().enumerate() {
        let need = DIRENT + name.len() as u64;
        if used + need > BLOCK && used > 0 {
            ends.push(i);
            used = 0;
        }
        used += need;
    }
    if used > 0 {
        ends.push(entries.len());
    }
    let size = (ends.len() as u64).saturating_sub(1) * BLOCK + used;
    (ends, size)
}

/// Writes in order, knowing where it is: the metadata area goes out as it is laid out,
/// its records at rising offsets, not built whole first (audit D12).
struct Seq<'o> {
    out: &'o mut dyn Write,
    at: u64,
}

impl Seq<'_> {
    /// Zeros up to `to`.
    fn pad_to(&mut self, to: u64) -> Result<(), Error> {
        if to < self.at {
            return err(format!("metadata at {to} written after {}", self.at));
        }
        while self.at < to {
            let n = (to - self.at).min(BLOCK);
            self.put(ZEROS.get(..n as usize).unwrap_or_default())?;
        }
        Ok(())
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.out.write_all(bytes)?;
        self.at += bytes.len() as u64;
        Ok(())
    }
}

/// Writes `tree` as an EROFS image to `out`, reading file contents from `source`.
pub fn write(tree: &Tree, source: &mut dyn Source, out: &mut dyn Write) -> Result<Written, Error> {
    // Depth-first order from the root, children by name: every node once, at its first
    // name, so the root is first and hard links share one inode.
    let mut order: Vec<Inode<'_>> = Vec::new();
    let mut index: HashMap<NodeId, usize> = HashMap::new();
    let mut stack: Vec<(NodeId, NodeId)> = vec![(Tree::ROOT, Tree::ROOT)];
    while let Some((id, parent)) = stack.pop() {
        if let Some(&i) = index.get(&id) {
            if let Some(inode) = order.get_mut(i) {
                inode.nlink += 1;
            }
            continue;
        }
        let node = tree.node(id).ok_or_else(|| Error(format!("missing node {id}")))?;
        index.insert(id, order.len());
        order.push(Inode {
            node: id,
            nlink: 1,
            parent,
            extended: false,
            xattrs: xattr_len(&node.meta.xattrs),
            size: 0,
            tail: 0,
            inline: false,
            nid: 0,
            start: None,
            entries: Vec::new(),
            block_ends: Vec::new(),
        });
        if let Kind::Dir(entries) = &node.kind {
            // Reverse, so the stack yields names in order.
            for (_, &child) in entries.iter().rev() {
                stack.push((child, id));
            }
        }
    }

    // Link counts, sizes, and each directory's blocks.
    let mut subdirs: HashMap<NodeId, u32> = HashMap::new();
    for inode in &order {
        if let Some(Node {
            kind: Kind::Dir(_), ..
        }) = tree.node(inode.node)
            && inode.node != Tree::ROOT
        {
            *subdirs.entry(inode.parent).or_default() += 1;
        }
    }
    let mut epoch = i64::MAX;
    for inode in &mut order {
        let node = tree
            .node(inode.node)
            .ok_or_else(|| Error("missing node".into()))?;
        epoch = epoch.min(node.meta.mtime);
        match &node.kind {
            Kind::Dir(entries) => {
                inode.nlink = 2 + subdirs.get(&inode.node).copied().unwrap_or(0);
                let mut all: Vec<Entry<'_>> = Vec::with_capacity(entries.len() + 2);
                all.extend(entries.iter().map(|(n, &c)| (n.as_slice(), c)));
                all.push((b".", inode.node));
                all.push((b"..", inode.parent));
                // Byte order, a prefix first: the kernel's lookup (namei.c) relies on it.
                all.sort_by(|a, b| a.0.cmp(b.0));
                let (ends, size) = dir_blocks(&all);
                inode.entries = all;
                inode.block_ends = ends;
                inode.size = size;
            }
            Kind::File { size, .. } => inode.size = *size,
            Kind::Symlink(target) => inode.size = target.len() as u64,
            _ => inode.size = 0,
        }
    }
    if epoch == i64::MAX {
        epoch = 0;
    }

    // Inode formats and the metadata area.
    let mut offset = SUPER_OFFSET + SUPER_SIZE;
    for inode in &mut order {
        let node = tree
            .node(inode.node)
            .ok_or_else(|| Error("missing node".into()))?;
        let m = &node.meta;
        let since_epoch = m.mtime.checked_sub(epoch).and_then(|d| u32::try_from(d).ok());
        inode.extended = m.uid > u32::from(u16::MAX)
            || m.gid > u32::from(u16::MAX)
            || inode.size > u64::from(u32::MAX)
            || inode.nlink > u32::from(u16::MAX)
            || since_epoch.is_none()
            || m.mtime_nsec != 0;
        let head = inode.isize() + inode.xattrs;
        let tail = inode.size % BLOCK;
        inode.inline = tail > 0 && head + tail <= BLOCK;
        inode.tail = if inode.inline { tail } else { 0 };
        let record = head + inode.tail;
        offset = offset.next_multiple_of(SLOT);
        // A record that fits in a block never crosses one; a larger one starts a block.
        if offset % BLOCK + record > BLOCK {
            offset = offset.next_multiple_of(BLOCK);
        }
        inode.nid = offset / SLOT;
        offset += record;
    }
    let root_nid = order.first().map_or(0, |i| i.nid);
    let root_nid = u16::try_from(root_nid).map_err(|_| Error("root inode placed too far".into()))?;
    let meta_blocks = offset.div_ceil(BLOCK);
    let mut next_block = meta_blocks;
    for inode in &mut order {
        let blocks = inode.data_blocks();
        if blocks > 0 {
            inode.start = Some(next_block);
            next_block += blocks;
        }
    }
    let total_blocks = next_block;
    let total =
        u32::try_from(total_blocks).map_err(|_| Error("image too large for 32-bit block numbers".into()))?;
    let nid_of = |id: NodeId| -> u64 { index.get(&id).and_then(|&i| order.get(i)).map_or(0, |i| i.nid) };

    // The metadata area: superblock, then inode records, each where it was placed.
    let mut seq = Seq { out, at: 0 };
    let mut sb = [0u8; SUPER_SIZE as usize];
    {
        let mut w = &mut sb[..];
        w.write_all(&MAGIC.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?; // checksum: SB_CHKSUM is off
        w.write_all(&0u32.to_le_bytes())?; // feature_compat
        w.write_all(&[BLOCK_BITS, 0])?; // blkszbits, sb_extslots
        w.write_all(&root_nid.to_le_bytes())?;
        w.write_all(&(order.len() as u64).to_le_bytes())?; // inos
        w.write_all(&epoch.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?; // fixed_nsec
        w.write_all(&total.to_le_bytes())?; // blocks_lo
        w.write_all(&0u32.to_le_bytes())?; // meta_blkaddr
        w.write_all(&0u32.to_le_bytes())?; // xattr_blkaddr
        // uuid, volume name, features: zero
    }
    seq.pad_to(SUPER_OFFSET)?;
    seq.put(&sb)?;

    // One buffer for file bytes, and one for a directory block, reused throughout.
    let mut buf = vec![0u8; 1 << 20];
    let mut block = Vec::with_capacity(BLOCK as usize);
    let mut xattrs = Vec::new();
    for (ino, inode) in order.iter().enumerate() {
        let node = tree
            .node(inode.node)
            .ok_or_else(|| Error("missing node".into()))?;
        let m = &node.meta;
        let (type_bits, rdev) = match &node.kind {
            Kind::Dir(_) => (ifmt::DIR, 0),
            Kind::File { .. } => (ifmt::REG, 0),
            Kind::Symlink(_) => (ifmt::LNK, 0),
            Kind::CharDevice { major, minor } => (ifmt::CHR, encode_dev(*major, *minor)?),
            Kind::BlockDevice { major, minor } => (ifmt::BLK, encode_dev(*major, *minor)?),
            Kind::Fifo => (ifmt::FIFO, 0),
            Kind::Socket => (ifmt::SOCK, 0),
        };
        let mode = type_bits | (m.mode & 0o7777);
        let layout = if inode.inline { FLAT_INLINE } else { FLAT_PLAIN };
        let format = u16::from(inode.extended) | (layout << 1);
        let i_u = match &node.kind {
            Kind::CharDevice { .. } | Kind::BlockDevice { .. } => rdev,
            Kind::Fifo | Kind::Socket => 0,
            _ => match inode.start {
                Some(b) => u32::try_from(b).map_err(|_| Error("block number overflow".into()))?,
                None => NULL_ADDR,
            },
        };
        xattr_body(&m.xattrs, &mut xattrs)?;
        let icount = if xattrs.is_empty() {
            0u16
        } else {
            u16::try_from(1 + (xattrs.len() as u64 - XATTR_HEADER) / 4)
                .map_err(|_| Error("too many xattrs".into()))?
        };
        let ino = u32::try_from(ino + 1).map_err(|_| Error("too many inodes".into()))?;
        let mut rec = [0u8; EXTENDED as usize];
        {
            let mut w = &mut rec[..];
            w.write_all(&format.to_le_bytes())?;
            w.write_all(&icount.to_le_bytes())?;
            w.write_all(&mode.to_le_bytes())?;
            if inode.extended {
                w.write_all(&0u16.to_le_bytes())?; // i_nb: nlink is below
                w.write_all(&inode.size.to_le_bytes())?;
                w.write_all(&i_u.to_le_bytes())?;
                w.write_all(&ino.to_le_bytes())?;
                w.write_all(&m.uid.to_le_bytes())?;
                w.write_all(&m.gid.to_le_bytes())?;
                w.write_all(&m.mtime.to_le_bytes())?;
                w.write_all(&m.mtime_nsec.to_le_bytes())?;
                w.write_all(&inode.nlink.to_le_bytes())?;
            } else {
                let since_epoch = (m.mtime - epoch) as u32;
                w.write_all(&(inode.nlink as u16).to_le_bytes())?;
                w.write_all(&(inode.size as u32).to_le_bytes())?;
                w.write_all(&since_epoch.to_le_bytes())?;
                w.write_all(&i_u.to_le_bytes())?;
                w.write_all(&ino.to_le_bytes())?;
                w.write_all(&(m.uid as u16).to_le_bytes())?;
                w.write_all(&(m.gid as u16).to_le_bytes())?;
            }
        }
        seq.pad_to(inode.nid * SLOT)?;
        seq.put(rec.get(..inode.isize() as usize).unwrap_or_default())?;
        seq.put(&xattrs)?;
        if inode.inline {
            let start = inode.size - inode.tail;
            let tail = inode.tail as usize;
            match &node.kind {
                Kind::Dir(_) => {
                    let last = inode
                        .dir_blocks()
                        .last()
                        .ok_or_else(|| Error("empty directory".into()))?;
                    dir_block(last, &nid_of, tree, &mut block);
                    seq.put(block.get(..tail).unwrap_or(&block))?;
                }
                Kind::File { data, .. } => {
                    let bytes = buf.get_mut(..tail).ok_or_else(|| Error("buffer".into()))?;
                    source.read_at(*data, start, bytes)?;
                    seq.put(bytes)?;
                }
                Kind::Symlink(target) => seq.put(target.get(start as usize..).unwrap_or_default())?,
                _ => {}
            }
        }
    }
    seq.pad_to(meta_blocks * BLOCK)?;

    // Data blocks, in inode order.
    for inode in &order {
        let Some(_) = inode.start else { continue };
        let node = tree
            .node(inode.node)
            .ok_or_else(|| Error("missing node".into()))?;
        let full = inode.data_blocks() * BLOCK;
        let end = seq.at + full;
        match &node.kind {
            Kind::Dir(_) => {
                for (i, entries) in inode.dir_blocks().enumerate() {
                    if (i as u64) * BLOCK >= full {
                        break;
                    }
                    dir_block(entries, &nid_of, tree, &mut block);
                    let to = seq.at + BLOCK;
                    seq.put(&block)?;
                    seq.pad_to(to)?;
                }
            }
            Kind::File { data, .. } => {
                let mut at = 0u64;
                let stop = full.min(inode.size);
                while at < stop {
                    let n = (stop - at).min(buf.len() as u64) as usize;
                    let chunk = buf.get_mut(..n).ok_or_else(|| Error("buffer".into()))?;
                    source.read_at(*data, at, chunk)?;
                    seq.put(chunk)?;
                    at += n as u64;
                }
            }
            Kind::Symlink(target) => seq.put(target.get(..full as usize).unwrap_or(target))?,
            _ => {}
        }
        seq.pad_to(end)?;
    }
    Ok(Written {
        blocks: total_blocks,
        inodes: order.len() as u64,
    })
}

/// One directory block into `bytes`: dirents, then the names, unpadded.
fn dir_block(entries: &[Entry<'_>], nid_of: &dyn Fn(NodeId) -> u64, tree: &Tree, bytes: &mut Vec<u8>) {
    bytes.clear();
    let mut nameoff = entries.len() as u64 * DIRENT;
    for (name, id) in entries {
        let file_type = match tree.node(*id).map(|n| &n.kind) {
            Some(Kind::Dir(_)) => ft::DIR,
            Some(Kind::File { .. }) => ft::REG,
            Some(Kind::Symlink(_)) => ft::LNK,
            Some(Kind::CharDevice { .. }) => ft::CHR,
            Some(Kind::BlockDevice { .. }) => ft::BLK,
            Some(Kind::Fifo) => ft::FIFO,
            Some(Kind::Socket) => ft::SOCK,
            None => 0,
        };
        bytes.extend_from_slice(&nid_of(*id).to_le_bytes());
        bytes.extend_from_slice(&(nameoff as u16).to_le_bytes());
        bytes.push(file_type);
        bytes.push(0);
        nameoff += name.len() as u64;
    }
    for (name, _) in entries {
        bytes.extend_from_slice(name);
    }
}

/// The kernel's new_encode_dev, for the 12-bit majors and 20-bit minors a dev_t holds
/// (include/linux/kdev_t.h).
fn encode_dev(major: u32, minor: u32) -> Result<u32, Error> {
    if major > 0xfff || minor > 0xf_ffff {
        return err(format!("device {major}:{minor} is out of Linux's range"));
    }
    Ok((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    /// File contents for tests, by source index.
    struct Mem(Vec<Vec<u8>>);

    impl Source for Mem {
        fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
            let bytes = &self.0[data.source as usize];
            let start = (data.offset + at) as usize;
            buf.copy_from_slice(&bytes[start..start + buf.len()]);
            Ok(())
        }
    }

    /// A reader written from the kernel's rules, independent of the writer's code.
    struct Reader<'a> {
        b: &'a [u8],
        root: u64,
        epoch: i64,
    }

    #[derive(Debug)]
    struct RInode {
        nid: u64,
        mode: u16,
        uid: u32,
        gid: u32,
        nlink: u32,
        size: u64,
        mtime: i64,
        nsec: u32,
        inline: bool,
        start: u32,
        xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
        tail_at: usize,
    }

    fn le16(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
    }
    fn le32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn le64(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    impl<'a> Reader<'a> {
        fn open(b: &'a [u8]) -> Reader<'a> {
            let sb = SUPER_OFFSET as usize;
            assert_eq!(le32(b, sb), MAGIC);
            assert_eq!(b[sb + 12], BLOCK_BITS);
            assert_eq!(le32(b, sb + 8), 0, "no compat features");
            assert_eq!(le32(b, sb + 80), 0, "no incompat features");
            assert_eq!(b.len() as u64, u64::from(le32(b, sb + 36)) * BLOCK, "blocks_lo");
            Reader {
                b,
                root: u64::from(le16(b, sb + 14)),
                epoch: le64(b, sb + 24) as i64,
            }
        }

        fn inode(&self, nid: u64) -> RInode {
            assert_ne!(nid, 0, "readdir hides inode 0");
            let at = (nid * SLOT) as usize;
            let b = self.b;
            let format = le16(b, at);
            let extended = format & 1 == 1;
            let layout = (format >> 1) & 7;
            assert!(layout == FLAT_PLAIN || layout == FLAT_INLINE, "layout {layout}");
            let icount = le16(b, at + 2);
            let xsize = if icount == 0 {
                0
            } else {
                12 + 4 * (usize::from(icount) - 1)
            };
            let (isize, mode, size, start, uid, gid, mtime, nsec, nlink);
            if extended {
                isize = 64;
                mode = le16(b, at + 4);
                size = le64(b, at + 8);
                start = le32(b, at + 16);
                uid = le32(b, at + 24);
                gid = le32(b, at + 28);
                mtime = le64(b, at + 32) as i64;
                nsec = le32(b, at + 40);
                nlink = le32(b, at + 44);
            } else {
                isize = 32;
                mode = le16(b, at + 4);
                nlink = u32::from(le16(b, at + 6));
                size = u64::from(le32(b, at + 8));
                mtime = self.epoch + i64::from(le32(b, at + 12));
                nsec = 0;
                start = le32(b, at + 16);
                uid = u32::from(le16(b, at + 24));
                gid = u32::from(le16(b, at + 26));
            }
            let mut xattrs = BTreeMap::new();
            let mut x = at + isize + 12;
            while x < at + isize + xsize {
                let (nlen, idx, vlen) = (b[x] as usize, b[x + 1], le16(b, x + 2) as usize);
                let prefix: &[u8] = match idx {
                    1 => b"user.",
                    2 => b"system.posix_acl_access",
                    3 => b"system.posix_acl_default",
                    4 => b"trusted.",
                    5 => b"lustre.",
                    6 => b"security.",
                    _ => panic!("xattr index {idx}"),
                };
                let mut name = prefix.to_vec();
                name.extend_from_slice(&b[x + 4..x + 4 + nlen]);
                xattrs.insert(name, b[x + 4 + nlen..x + 4 + nlen + vlen].to_vec());
                x += (4 + nlen + vlen).next_multiple_of(4);
            }
            let tail_at = at + isize + xsize;
            let inline = layout == FLAT_INLINE;
            if inline {
                // data.c: inline data never crosses a block.
                let tail = size % BLOCK;
                assert!(
                    (tail_at as u64 % BLOCK) + tail <= BLOCK,
                    "tail crosses a block @ nid {nid}"
                );
            }
            RInode {
                nid,
                mode,
                uid,
                gid,
                nlink,
                size,
                mtime,
                nsec,
                inline,
                start,
                xattrs,
                tail_at,
            }
        }

        fn data(&self, i: &RInode) -> Vec<u8> {
            let full = if i.inline { i.size / BLOCK * BLOCK } else { i.size };
            let mut out = Vec::with_capacity(i.size as usize);
            if full > 0 {
                let s = u64::from(i.start) * BLOCK;
                out.extend_from_slice(&self.b[s as usize..(s + full) as usize]);
            }
            if i.inline {
                let tail = (i.size % BLOCK) as usize;
                out.extend_from_slice(&self.b[i.tail_at..i.tail_at + tail]);
            }
            out
        }

        /// Directory entries (name, nid, file type), checking the order lookups rely on.
        fn dir(&self, i: &RInode) -> Vec<(Vec<u8>, u64, u8)> {
            let data = self.data(i);
            let mut out: Vec<(Vec<u8>, u64, u8)> = Vec::new();
            for block in data.chunks(BLOCK as usize) {
                let n = usize::from(le16(block, 8)) / DIRENT as usize;
                for k in 0..n {
                    let d = k * DIRENT as usize;
                    let nid = le64(block, d);
                    let off = usize::from(le16(block, d + 8));
                    let end = if k + 1 < n {
                        usize::from(le16(block, d + DIRENT as usize + 8))
                    } else {
                        block[off..]
                            .iter()
                            .position(|&c| c == 0)
                            .map_or(block.len(), |p| off + p)
                    };
                    let name = block[off..end].to_vec();
                    assert!((1..=NAME_MAX).contains(&name.len()));
                    if let Some(prev) = out.last() {
                        assert!(prev.0 < name, "unsorted: {:?} then {:?}", prev.0, name);
                    }
                    out.push((name, nid, block[d + 10]));
                }
            }
            out
        }

        fn lookup(&self, path: &str) -> RInode {
            let mut cur = self.inode(self.root);
            for part in path.split('/').filter(|p| !p.is_empty()) {
                let nid = self
                    .dir(&cur)
                    .into_iter()
                    .find(|(n, _, _)| n == part.as_bytes())
                    .unwrap_or_else(|| panic!("{path}: no {part}"))
                    .1;
                cur = self.inode(nid);
            }
            cur
        }
    }

    fn meta(mode: u16) -> Meta {
        Meta {
            mode,
            uid: 0,
            gid: 0,
            mtime: 1_700_000_000,
            mtime_nsec: 0,
            xattrs: BTreeMap::new(),
        }
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// Files around every block boundary, a long symlink, a directory of several blocks
    /// with names that sort before `.`, devices, links and xattrs.
    fn sample() -> (Tree, Mem) {
        let mut tree = Tree::new(meta(0o755));
        let mut files = Vec::new();
        let etc = tree
            .insert(
                Tree::ROOT,
                b"etc",
                Node {
                    kind: Kind::Dir(BTreeMap::new()),
                    meta: meta(0o755),
                },
            )
            .unwrap();
        for (i, len) in [0usize, 1, 4095, 4096, 4097, 3 * 4096 + 17, (1 << 20) + 5]
            .into_iter()
            .enumerate()
        {
            files.push(pattern(len, i as u8));
            let data = DataRef {
                source: i as u32,
                offset: 0,
            };
            tree.insert(
                etc,
                format!("f{len}").as_bytes(),
                Node {
                    kind: Kind::File {
                        size: len as u64,
                        data,
                    },
                    meta: meta(0o644),
                },
            )
            .unwrap();
        }
        let big = tree
            .insert(
                Tree::ROOT,
                b"many",
                Node {
                    kind: Kind::Dir(BTreeMap::new()),
                    meta: meta(0o700),
                },
            )
            .unwrap();
        for i in 0..600 {
            tree.insert(
                big,
                format!("entry-{i:04}").as_bytes(),
                Node {
                    kind: Kind::Fifo,
                    meta: meta(0o600),
                },
            )
            .unwrap();
        }
        for name in [&b"-dash"[..], b"!bang", b"#hash", &[b'z'; 255][..]] {
            tree.insert(
                big,
                name,
                Node {
                    kind: Kind::Socket,
                    meta: meta(0o600),
                },
            )
            .unwrap();
        }
        tree.insert(
            Tree::ROOT,
            b"short",
            Node {
                kind: Kind::Symlink(b"etc/f1".to_vec()),
                meta: meta(0o777),
            },
        )
        .unwrap();
        tree.insert(
            Tree::ROOT,
            b"long",
            Node {
                kind: Kind::Symlink(vec![b'x'; 5000]),
                meta: meta(0o777),
            },
        )
        .unwrap();
        tree.insert(
            Tree::ROOT,
            b"tty",
            Node {
                kind: Kind::CharDevice { major: 5, minor: 300 },
                meta: meta(0o620),
            },
        )
        .unwrap();
        tree.insert(
            Tree::ROOT,
            b"sda",
            Node {
                kind: Kind::BlockDevice { major: 8, minor: 1 },
                meta: meta(0o660),
            },
        )
        .unwrap();
        let shared = tree.child(etc, b"f4097").unwrap();
        tree.link(Tree::ROOT, b"hardlink", shared).unwrap();
        let mut x = meta(0o4755);
        x.uid = 100_000; // needs the extended inode
        x.gid = 70_000;
        x.mtime_nsec = 5;
        x.xattrs.insert(b"user.origin".to_vec(), b"shards".to_vec());
        x.xattrs.insert(b"security.capability".to_vec(), vec![1, 2, 3]);
        x.xattrs.insert(b"trusted.overlay.opaque".to_vec(), b"y".to_vec());
        x.xattrs
            .insert(b"system.posix_acl_access".to_vec(), vec![2, 0, 0, 0]);
        files.push(pattern(9000, 42));
        tree.insert(
            Tree::ROOT,
            b"suid",
            Node {
                kind: Kind::File {
                    size: 9000,
                    data: DataRef { source: 7, offset: 0 },
                },
                meta: x,
            },
        )
        .unwrap();
        (tree, Mem(files))
    }

    fn image(tree: &Tree, mem: &mut Mem) -> Vec<u8> {
        let mut out = Vec::new();
        write(tree, mem, &mut out).unwrap();
        out
    }

    #[test]
    fn everything_reads_back_as_written() {
        let (tree, mut mem) = sample();
        let img = image(&tree, &mut mem);
        let r = Reader::open(&img);
        assert_eq!(r.root, 36, "the root follows the superblock");
        let root = r.inode(r.root);
        assert_eq!(root.mode, ifmt::DIR | 0o755);
        assert_eq!(root.nlink, 4, "root, etc, many");
        for (i, len) in [0usize, 1, 4095, 4096, 4097, 3 * 4096 + 17, (1 << 20) + 5]
            .into_iter()
            .enumerate()
        {
            let f = r.lookup(&format!("etc/f{len}"));
            assert_eq!(f.size, len as u64);
            assert_eq!(r.data(&f), mem.0[i], "f{len}");
            assert_eq!(f.mode, ifmt::REG | 0o644);
            assert_eq!(f.mtime, 1_700_000_000);
        }
        let many = r.dir(&r.lookup("many"));
        assert_eq!(many.len(), 600 + 4 + 2);
        assert!(many.first().unwrap().0 < b".".to_vec(), "names sort before `.`");
        assert_eq!(r.data(&r.lookup("short")), b"etc/f1");
        assert_eq!(r.data(&r.lookup("long")), vec![b'x'; 5000]);
        let tty = r.lookup("tty");
        assert_eq!(
            (tty.mode, tty.start),
            (ifmt::CHR | 0o620, encode_dev(5, 300).unwrap())
        );
        let hard = r.lookup("hardlink");
        assert_eq!(hard.nid, r.lookup("etc/f4097").nid);
        assert_eq!(hard.nlink, 2);
        let suid = r.lookup("suid");
        assert_eq!(
            (suid.uid, suid.gid, suid.nsec, suid.mode),
            (100_000, 70_000, 5, ifmt::REG | 0o4755)
        );
        assert_eq!(r.data(&suid), mem.0[7]);
        assert_eq!(
            suid.xattrs,
            tree.node(tree.child(Tree::ROOT, b"suid").unwrap())
                .unwrap()
                .meta
                .xattrs
        );
        // `.` and `..` point where they should.
        let etc = r.lookup("etc");
        let dots: Vec<_> = r
            .dir(&etc)
            .into_iter()
            .filter(|(n, _, _)| n.starts_with(b"."))
            .collect();
        assert_eq!(
            dots,
            vec![
                (b".".to_vec(), etc.nid, ft::DIR),
                (b"..".to_vec(), r.root, ft::DIR)
            ]
        );
    }

    #[test]
    fn the_same_tree_gives_the_same_bytes() {
        let (tree, mut mem) = sample();
        assert_eq!(image(&tree, &mut mem), image(&tree, &mut mem));
    }

    #[test]
    fn removed_entries_are_not_written() {
        let (mut tree, mut mem) = sample();
        let before = image(&tree, &mut mem).len();
        let etc = tree.child(Tree::ROOT, b"etc").unwrap();
        tree.remove(etc, b"f1048581").unwrap();
        let after = image(&tree, &mut mem);
        assert!(after.len() < before, "a removed 1 MiB file still takes space");
        let r = Reader::open(&after);
        assert!(r.dir(&r.lookup("etc")).iter().all(|(n, _, _)| n != b"f1048581"));
    }

    #[test]
    fn invalid_names_and_links_are_refused() {
        let mut tree = Tree::new(meta(0o755));
        for bad in [&b""[..], b".", b"..", b"a/b", b"nul\0", &[b'n'; 256][..]] {
            assert!(
                tree.insert(
                    Tree::ROOT,
                    bad,
                    Node {
                        kind: Kind::Fifo,
                        meta: meta(0)
                    }
                )
                .is_err()
            );
        }
        let d = tree
            .insert(
                Tree::ROOT,
                b"d",
                Node {
                    kind: Kind::Dir(BTreeMap::new()),
                    meta: meta(0o755),
                },
            )
            .unwrap();
        assert!(
            tree.link(Tree::ROOT, b"loop", d).is_err(),
            "hard links to directories"
        );
        let mut x = meta(0o644);
        x.xattrs.insert(b"system.other".to_vec(), vec![]);
        tree.insert(
            Tree::ROOT,
            b"x",
            Node {
                kind: Kind::Fifo,
                meta: x,
            },
        )
        .unwrap();
        assert!(
            write(&tree, &mut Mem(vec![]), &mut Vec::new()).is_err(),
            "an unstorable xattr"
        );
    }
}
