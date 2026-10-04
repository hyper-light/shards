//! Writes EROFS images as Linux 6.18 reads them (fs/erofs/erofs_fs.h, inode.c, data.c,
//! dir.c, namei.c and xattr.c at v6.18.48): uncompressed, 4 KiB blocks, inline xattrs, and
//! no feature flags. The bytes are a function of the tree alone.
//!
//! A regular file's data is whole blocks (`FLAT_PLAIN`), so guests map it straight from
//! pmem with DAX: the kernel gives DAX only to plain and chunk-based files
//! (inode.c:194-198), and an inline tail would send every read through the guest's page
//! cache, copied out of pmem (docs/research/platform-measurements.md M74). Directories and
//! symlinks, which DAX never serves, keep their tails inline.
//!
//! Layout. Block 0 holds the superblock at byte 1024. The metadata area starts at block 0
//! (`meta_blkaddr` 0), so inode numbers (nid = byte offset / 32) start after the
//! superblock, at 36, as erofs-utils' do, and never 0: glibc's readdir skips entries whose
//! inode number is 0. Every inode record (inode, inline xattrs, inline tail) stays within
//! one block, as the kernel requires of inline data (data.c). Data blocks follow the
//! metadata, in inode order.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::Arc;

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

/// Ownership, permissions, times and extended attributes of a node: 32 bytes, as a
/// tree holds one for each of its files (platform-measurements.md M78).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// Permission and set-id bits (0o7777); the file type comes from the node's kind.
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub xattrs: Xattrs,
}

/// Extended attributes by full name, such as `user.foo` or `security.capability`, in
/// name order. None take a pointer's room: nearly every file has none, and an empty map
/// took 24 bytes of each node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
// Boxed so that none take 8 bytes, not a map's 24; the few nodes with some pay for it.
#[allow(clippy::box_collection)]
pub struct Xattrs(Option<Box<BTreeMap<Vec<u8>, Vec<u8>>>>);

impl Xattrs {
    pub fn get(&self, name: &[u8]) -> Option<&Vec<u8>> {
        self.0.as_ref()?.get(name)
    }

    pub fn insert(&mut self, name: Vec<u8>, value: Vec<u8>) -> Option<Vec<u8>> {
        self.0.get_or_insert_default().insert(name, value)
    }

    pub fn remove(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let map = self.0.as_mut()?;
        let gone = map.remove(name);
        if map.is_empty() {
            self.0 = None;
        }
        gone
    }

    /// Keeps the attributes `keep` says to.
    pub fn retain(&mut self, keep: impl FnMut(&Vec<u8>, &mut Vec<u8>) -> bool) {
        if let Some(map) = self.0.as_mut() {
            map.retain(keep);
            if map.is_empty() {
                self.0 = None;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    pub fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |m| m.len())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.0.iter().flat_map(|m| m.iter())
    }

    pub fn keys(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.iter().map(|(k, _)| k)
    }
}

impl FromIterator<(Vec<u8>, Vec<u8>)> for Xattrs {
    fn from_iter<I: IntoIterator<Item = (Vec<u8>, Vec<u8>)>>(iter: I) -> Xattrs {
        let map: BTreeMap<Vec<u8>, Vec<u8>> = iter.into_iter().collect();
        Xattrs((!map.is_empty()).then(|| Box::new(map)))
    }
}

/// Where a regular file's bytes are, for the [`Source`] that reads them. Packed to 12
/// bytes, so a file's [`Kind`] and its size take 24 with the variant's tag, not 32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed(4))]
pub struct DataRef {
    pub source: u32,
    pub offset: u64,
}

/// Reads file contents while the image is written.
pub trait Source {
    /// Fills `buf` with the file's bytes from `at`.
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// A directory's entries, which only its [`Tree`] reads or changes: the head of its list
/// of entries, how many are live, and the entry that names the directory itself, which
/// a directory has one of. [`Dir::default`] is an empty directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dir {
    first: u32,
    live: u32,
    up: u32,
}

impl Default for Dir {
    fn default() -> Dir {
        Dir {
            first: NONE,
            live: 0,
            up: NONE,
        }
    }
}

/// An entry of a [`Tree`], by id: a name in a directory.
pub type EntryId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir(Dir),
    File {
        size: u64,
        data: DataRef,
    },
    /// Its target: boxed, 16 bytes, not a vector's 24.
    Symlink(Box<[u8]>),
    CharDevice {
        major: u32,
        minor: u32,
    },
    BlockDevice {
        major: u32,
        minor: u32,
    },
    Fifo,
    Socket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: Kind,
    pub meta: Meta,
}

/// No node, no entry: ids are `u32`, and this one is never given.
const NONE: u32 = u32::MAX;

/// Items by id, in chunks of [`Arena::CHUNK`], each shared by the trees cloned from one
/// another until one of them changes it (review 4.2): a clone copies the chunks' handles
/// alone, and a change copies the one chunk it lands in, so a build's snapshots, each a
/// step's clone of the one before, hold what their steps changed and share the rest
/// (platform-measurements.md M108). Growing moves only the last chunk's items, and a
/// chunk grows as a vector does, so a small tree holds a small one (M78).
#[derive(Debug, Clone)]
struct Arena<T> {
    chunks: Vec<Arc<Vec<T>>>,
    len: usize,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            chunks: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Clone> Arena<T> {
    const CHUNK_BITS: u32 = 10;
    const CHUNK: usize = 1 << Self::CHUNK_BITS;

    /// `len` copies of `item`.
    fn filled(len: usize, item: T) -> Arena<T> {
        let chunks = (0..len.div_ceil(Self::CHUNK))
            .map(|c| Arc::new(vec![item.clone(); (len - c * Self::CHUNK).min(Self::CHUNK)]))
            .collect();
        Arena { chunks, len }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, id: usize) -> Option<&T> {
        self.chunks
            .get(id >> Self::CHUNK_BITS)?
            .get(id & (Self::CHUNK - 1))
    }

    /// The item to change: its chunk this tree's own first, copied if another shares it.
    fn get_mut(&mut self, id: usize) -> Option<&mut T> {
        Arc::make_mut(self.chunks.get_mut(id >> Self::CHUNK_BITS)?).get_mut(id & (Self::CHUNK - 1))
    }

    /// Adds `item`, returning its id, which fits a `u32` short of [`NONE`].
    fn push(&mut self, item: T) -> Result<u32, Error> {
        let id = u32::try_from(self.len)
            .ok()
            .filter(|&id| id != NONE)
            .ok_or_else(|| Error("too many nodes or entries for one tree".into()))?;
        match self.chunks.last_mut() {
            Some(chunk) if chunk.len() < Self::CHUNK => {
                // Doubling from a power of two stops at CHUNK, never past it.
                Arc::make_mut(chunk).push(item);
            }
            _ => self.chunks.push(Arc::new(vec![item])),
        }
        self.len += 1;
        Ok(id)
    }
}

/// A tree's names, each where it was added, one length byte and then the name, in
/// chunks of [`Names::CHUNK`] bytes a name never straddles, shared as an [`Arena`]'s
/// chunks are: a name's place is its chunk's number, and its offset in the chunk.
#[derive(Debug, Clone, Default)]
struct Names {
    chunks: Vec<Arc<Vec<u8>>>,
}

impl Names {
    const CHUNK_BITS: u32 = 16;
    const CHUNK: usize = 1 << Self::CHUNK_BITS;

    /// The name at `at`.
    fn get(&self, at: u32) -> &[u8] {
        let off = at as usize & (Self::CHUNK - 1);
        self.chunks
            .get((at >> Self::CHUNK_BITS) as usize)
            .and_then(|chunk| {
                let len = usize::from(*chunk.get(off)?);
                chunk.get(off + 1..off + 1 + len)
            })
            .unwrap_or_default()
    }

    /// Adds `name`, of at most [`NAME_MAX`] bytes, returning where it is.
    fn push(&mut self, name: &[u8]) -> Result<u32, Error> {
        let len = u8::try_from(name.len()).map_err(|_| Error("a name past NAME_MAX".into()))?;
        if self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.len() + 1 + name.len() > Self::CHUNK)
        {
            self.chunks.push(Arc::new(Vec::new()));
        }
        let number = u32::try_from(self.chunks.len() - 1)
            .ok()
            .filter(|&n| n >> (32 - Self::CHUNK_BITS) == 0)
            .ok_or_else(|| Error("too many names for one tree".into()))?;
        let chunk = Arc::make_mut(self.chunks.last_mut().ok_or_else(|| Error("no names".into()))?);
        let at = (number << Self::CHUNK_BITS) | chunk.len() as u32;
        chunk.push(len);
        chunk.extend_from_slice(name);
        Ok(at)
    }
}

/// A name in a directory: what it names, its bytes in the tree's names, and the next
/// entry of the same directory. A removed entry names [`NONE`], and stays until the tree
/// is compacted.
#[derive(Debug, Clone, Copy)]
struct Link {
    dir: u32,
    child: u32,
    /// Where the name starts in [`Tree::names`]: one length byte, then the name.
    name: u32,
    next: u32,
    /// The last step [`Tree::mark`] stamped the entry with: a step that made, changed or
    /// removed it, or something below it. Only `mark` stamps, so a stamped entry's
    /// directories are stamped too.
    stamp: u32,
}

/// Entries by directory and name: open addressing with linear probing (Knuth, TAOCP
/// vol. 3, 6.4, algorithm L) over entry ids, kept at most three quarters full. Names come
/// from archives no one vouches for, so the hash is SipHash with keys drawn for each
/// process (std's `RandomState`): a chosen set of names cannot pile onto one slot.
/// Removed entries keep their slots, so the probes past them still run, until the
/// table is rebuilt. A slot holds the entry's id alone, 4 bytes: a probe reads the entry
/// it names, and its name when the directory agrees.
#[derive(Debug, Clone, Default)]
struct Index {
    slots: Arena<u32>,
    used: usize,
    keys: std::hash::RandomState,
}

/// An empty slot: no entry has id [`NONE`].
const EMPTY: u32 = NONE;

impl Index {
    fn hash(&self, dir: u32, name: &[u8]) -> u64 {
        use std::hash::{BuildHasher, Hasher};
        let mut h = self.keys.build_hasher();
        h.write_u32(dir);
        h.write(name);
        h.finish()
    }
}

/// A directory tree to write. Nodes removed from it stay in the arena, unwritten, until
/// [`Tree::compact`]: only what the root reaches is written.
///
/// A directory's entries live in the tree, not in its node: each is a [`Link`], found by
/// directory and name through one hash index and listed through the directory's chain,
/// and sorted only when listed, as the image and a layer list them. A tree of a million
/// entries holds one entry record, one name and a slot each (M78).
#[derive(Debug)]
pub struct Tree {
    nodes: Arena<Node>,
    links: Arena<Link>,
    names: Names,
    index: Index,
    /// Entries replaced or removed since the last compaction: what may have left nodes
    /// unreachable.
    dropped: usize,
    /// The step [`Tree::mark`] stamps entries with: entries made, changed or removed in
    /// it, and the directories above them, are what the step changed. Steps start at 1:
    /// an entry never marked has 0.
    step: u32,
    /// This tree's identity, as an inode's generation is one's (ext4, XFS): fresh for every
    /// tree made, compacted or cloned, so that what was learned of one tree's node ids is
    /// never taken for another's. A compacted tree's ids mean other nodes.
    version: u64,
    /// The version of the tree this one was cloned from, or 0.
    parent: u64,
}

/// A version no tree has had: one counter for the process, never reused.
fn fresh_version() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Clone for Tree {
    /// A copy is a tree of its own: a fresh version, and the original's as its parent.
    fn clone(&self) -> Tree {
        Tree {
            nodes: self.nodes.clone(),
            links: self.links.clone(),
            names: self.names.clone(),
            index: self.index.clone(),
            dropped: self.dropped,
            step: self.step,
            version: fresh_version(),
            parent: self.version,
        }
    }
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
        let mut t = Tree {
            nodes: Arena::default(),
            links: Arena::default(),
            names: Names::default(),
            index: Index::default(),
            dropped: 0,
            step: 0,
            version: fresh_version(),
            parent: 0,
        };
        // The first of an empty arena: id 0 always fits.
        let _ = t.nodes.push(Node {
            kind: Kind::Dir(Dir::default()),
            meta: root,
        });
        t
    }

    /// How many nodes the arena holds, reachable or not.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// The ids below `end` of the nodes this tree and `other` may hold differently, in
    /// runs: all of them but those in chunks both still share, which neither has changed
    /// since one was cloned from the other, and so hold alike (review 4.3). A step's
    /// snapshot, compared with the one it began from, differs in the chunks it changed.
    pub fn unshared_nodes<'t>(
        &'t self,
        other: &'t Tree,
        end: usize,
    ) -> impl Iterator<Item = std::ops::Range<usize>> + 't {
        let chunk = Arena::<Node>::CHUNK;
        (0..end.div_ceil(chunk))
            .filter(
                move |&c| match (self.nodes.chunks.get(c), other.nodes.chunks.get(c)) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    _ => true,
                },
            )
            .map(move |c| c * chunk..((c + 1) * chunk).min(end))
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() == 0
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id)
    }

    fn dir(&self, id: NodeId) -> Option<Dir> {
        match self.nodes.get(id).map(|n| &n.kind) {
            Some(Kind::Dir(d)) => Some(*d),
            _ => None,
        }
    }

    fn dir_mut(&mut self, id: NodeId) -> Result<&mut Dir, Error> {
        match self.nodes.get_mut(id).map(|n| &mut n.kind) {
            Some(Kind::Dir(d)) => Ok(d),
            _ => err("not a directory"),
        }
    }

    fn name_of(&self, link: &Link) -> &[u8] {
        self.names.get(link.name)
    }

    /// The live entries of `dir` into `out`, by id, sorted by name.
    fn link_ids(&self, dir: NodeId, out: &mut Vec<u32>) {
        out.clear();
        let Some(d) = self.dir(dir) else { return };
        let mut at = d.first;
        while let Some(l) = self.links.get(at as usize) {
            if l.child != NONE {
                out.push(at);
            }
            at = l.next;
        }
        let name = |id: &u32| self.links.get(*id as usize).map_or(&[][..], |l| self.name_of(l));
        out.sort_unstable_by(|a, b| name(a).cmp(name(b)));
    }

    /// The live entry `name` of `dir`, by id.
    fn find(&self, dir: u32, name: &[u8]) -> Option<u32> {
        let mask = self.index.slots.len().checked_sub(1)?;
        let hash = self.index.hash(dir, name);
        let mut i = (hash as usize) & mask;
        loop {
            let id = *self.index.slots.get(i)?;
            if id == EMPTY {
                return None;
            }
            if let Some(l) = self.links.get(id as usize)
                && l.dir == dir
                && l.child != NONE
                && self.name_of(l) == name
            {
                return Some(id);
            }
            i = (i + 1) & mask;
        }
    }

    /// Puts entry `id` in the index, growing it first if it would pass three quarters.
    fn index_add(&mut self, id: u32) -> Result<(), Error> {
        if (self.index.used + 1) * 4 > self.index.slots.len() * 3 {
            let want = (self.index.slots.len() * 2).max(16);
            self.rebuild_index(want)?;
        }
        self.place(id)
    }

    fn place(&mut self, id: u32) -> Result<(), Error> {
        let link = *self
            .links
            .get(id as usize)
            .ok_or_else(|| Error("missing entry".into()))?;
        let mask = self.index.slots.len().saturating_sub(1);
        let hash = self.index.hash(link.dir, self.name_of(&link));
        let mut i = (hash as usize) & mask;
        loop {
            match self.index.slots.get_mut(i) {
                Some(s) if *s == EMPTY => {
                    *s = id;
                    self.index.used += 1;
                    return Ok(());
                }
                Some(_) => i = (i + 1) & mask,
                None => return err("entry index full"),
            }
        }
    }

    /// The index again, `slots` long, of the live entries only.
    fn rebuild_index(&mut self, slots: usize) -> Result<(), Error> {
        let slots = slots.next_power_of_two();
        self.index.slots = Arena::filled(slots, EMPTY);
        self.index.used = 0;
        for id in 0..self.links.len() {
            if self.links.get(id).is_some_and(|l| l.child != NONE) {
                self.place(id as u32)?;
            }
        }
        Ok(())
    }

    /// A new entry `name` in `dir`, naming `child`, at the head of the directory's list.
    fn add_link(&mut self, dir: u32, name: &[u8], child: u32) -> Result<(), Error> {
        let next = self.dir_mut(dir as usize)?.first;
        let at = self.names.push(name)?;
        let id = self.links.push(Link {
            dir,
            child,
            name: at,
            next,
            stamp: 0,
        })?;
        let head = self.dir_mut(dir as usize)?;
        head.first = id;
        head.live += 1;
        self.adopt(child, id);
        self.index_add(id)
    }

    /// Records that entry `link` names `child`, if `child` is a directory.
    fn adopt(&mut self, child: u32, link: u32) {
        if let Some(Node {
            kind: Kind::Dir(d), ..
        }) = self.nodes.get_mut(child as usize)
        {
            d.up = link;
        }
    }

    /// Sets the entry `name` of `dir` to `child`: the live one replaced, or a new one.
    fn set(&mut self, dir: NodeId, name: &[u8], child: u32) -> Result<(), Error> {
        check_name(name)?;
        let dir = u32::try_from(dir).map_err(|_| Error("not a directory".into()))?;
        self.dir_mut(dir as usize)?;
        match self.find(dir, name) {
            Some(id) => {
                if let Some(l) = self.links.get_mut(id as usize) {
                    l.child = child;
                }
                self.adopt(child, id);
                self.dropped += 1;
                Ok(())
            }
            None => self.add_link(dir, name, child),
        }
    }

    /// Drops the live entry `id`: it keeps its place in the list and index, naming nothing.
    fn unset(&mut self, id: u32) -> Option<NodeId> {
        let l = self.links.get_mut(id as usize)?;
        let (dir, child) = (l.dir, l.child);
        l.child = NONE;
        if let Ok(d) = self.dir_mut(dir as usize) {
            d.live = d.live.saturating_sub(1);
        }
        self.dropped += 1;
        Some(child as NodeId)
    }

    /// Adds `node` as a new node: a directory starts empty, whatever tree it came from.
    fn new_node(&mut self, mut node: Node) -> Result<u32, Error> {
        if let Kind::Dir(d) = &mut node.kind {
            *d = Dir::default();
        }
        self.nodes.push(node)
    }

    /// This tree's identity ([`Tree`]'s `version`).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The version of the tree this one was cloned from, or 0.
    pub fn parent(&self) -> u64 {
        self.parent
    }

    /// Starts a new step: nothing is stamped with it yet. Steps outnumbering a `u32`
    /// start again from 1, every entry's stamp cleared first, so no old stamp is taken for
    /// the new step's.
    pub fn next_step(&mut self) {
        match self.step.checked_add(1) {
            Some(step) => self.step = step,
            None => {
                for id in 0..self.links.len() {
                    if let Some(l) = self.links.get_mut(id) {
                        l.stamp = 0;
                    }
                }
                self.step = 1;
            }
        }
    }

    /// The live entry `name` of `dir`, and what it names.
    pub fn lookup(&self, dir: NodeId, name: &[u8]) -> Option<(EntryId, NodeId)> {
        let d = u32::try_from(dir).ok()?;
        self.dir(dir)?;
        let id = self.find(d, name)?;
        self.links.get(id as usize).map(|l| (id, l.child as NodeId))
    }

    /// The directory an entry is in, and its name.
    pub fn entry(&self, id: EntryId) -> Option<(NodeId, &[u8])> {
        let l = self.links.get(id as usize)?;
        Some((l.dir as NodeId, self.name_of(l)))
    }

    /// The entry naming directory `dir`, or `None` for the root and what is no directory.
    pub fn up(&self, dir: NodeId) -> Option<EntryId> {
        self.dir(dir).map(|d| d.up).filter(|&u| u != NONE)
    }

    /// Stamps entry `id` with this step, and the entries naming the directories above it,
    /// up to the first already stamped: what overlayfs copies up to change it.
    pub fn mark(&mut self, id: EntryId) {
        let step = self.step;
        let mut at = id;
        while let Some(l) = self.links.get_mut(at as usize) {
            if l.stamp == step && at != id {
                return;
            }
            l.stamp = step;
            let dir = l.dir as usize;
            match self.up(dir) {
                Some(up) => at = up,
                None => return,
            }
        }
    }

    /// Stamps the entries naming `dir` and the directories above it, as [`Tree::mark`].
    pub fn mark_dir(&mut self, dir: NodeId) {
        if let Some(up) = self.up(dir) {
            self.mark(up);
        }
    }

    /// The step under way: what [`Tree::mark`] stamps now.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// What this step changed in `dir`, into `out`: each name stamped with it, once, and
    /// what it names now, or `None` where it was removed; sorted by name.
    pub fn changed_into<'t>(&'t self, dir: NodeId, out: &mut Vec<(&'t [u8], Option<NodeId>)>) {
        self.changed_at(dir, self.step, out);
    }

    /// What step `step` changed in `dir` and no later step changed again, as
    /// [`Tree::changed_into`] lists it.
    pub fn changed_at<'t>(&'t self, dir: NodeId, step: u32, out: &mut Vec<(&'t [u8], Option<NodeId>)>) {
        out.clear();
        // Before the first step nothing was marked, and 0 is every unmarked entry's stamp.
        if step == 0 {
            return;
        }
        let Some(d) = self.dir(dir) else { return };
        let mut at = d.first;
        while let Some(l) = self.links.get(at as usize) {
            if l.stamp == step {
                out.push((self.name_of(l), (l.child != NONE).then_some(l.child as NodeId)));
            }
            at = l.next;
        }
        // A name removed and made again in one step is listed once, as it is now.
        out.sort_unstable_by(|a, b| a.0.cmp(b.0).then(b.1.is_some().cmp(&a.1.is_some())));
        out.dedup_by(|b, a| a.0 == b.0);
    }

    /// The entry `name` in directory `dir`.
    pub fn child(&self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        let dir = u32::try_from(dir).ok()?;
        self.dir(dir as usize)?;
        let id = self.find(dir, name)?;
        self.links.get(id as usize).map(|l| l.child as NodeId)
    }

    /// How many entries `dir` holds, or `None` if it is no directory.
    pub fn dir_len(&self, dir: NodeId) -> Option<usize> {
        self.dir(dir).map(|d| d.live as usize)
    }

    /// The entries of `dir` into `out`, sorted by name in byte order, as the image and a
    /// layer list them; false if `dir` is no directory.
    pub fn entries_into<'t>(&'t self, dir: NodeId, out: &mut Vec<(&'t [u8], NodeId)>) -> bool {
        out.clear();
        let Some(d) = self.dir(dir) else {
            return false;
        };
        out.reserve(d.live as usize);
        let mut at = d.first;
        while let Some(l) = self.links.get(at as usize) {
            if l.child != NONE {
                out.push((self.name_of(l), l.child as NodeId));
            }
            at = l.next;
        }
        // Live names are unique in their directory.
        out.sort_unstable_by(|a, b| a.0.cmp(b.0));
        true
    }

    /// The entries of `dir`, sorted by name; empty if it is no directory.
    pub fn entries(&self, dir: NodeId) -> Vec<(&[u8], NodeId)> {
        let mut out = Vec::new();
        self.entries_into(dir, &mut out);
        out
    }

    /// Lets the index of names go, for a tree that is only listed from now on, as
    /// [`write`] lists it: after its nodes, entries and names, the index is the tree's
    /// largest part. Looking a name up finds nothing afterwards.
    pub fn drop_index(&mut self) {
        self.index.slots = Arena::default();
        self.index.used = 0;
    }

    /// [`compact`](Self::compact), once as many entries have been dropped since the last
    /// as half the nodes the tree holds: the copying, then, costs no more in all than
    /// dropping them did, and the tree holds at most twice what it needs, where compacting
    /// after every change costs a whole copy each time.
    pub fn compact_if_worth_it(&mut self) {
        if self.dropped.saturating_mul(2) >= self.nodes.len() {
            self.compact();
        }
    }

    /// Drops the nodes the root no longer reaches, and renumbers the rest from the root,
    /// depth first, so ids held from before mean nothing after. A node with several
    /// names, a hard link, stays one node (audit D11). Removed entries and the names of
    /// replaced ones go too. Nothing is done if no entry was replaced or removed since the
    /// last compaction.
    pub fn compact(&mut self) {
        if self.dropped == 0 {
            return;
        }
        let mut old = std::mem::replace(self, Tree::new(Meta::default()));
        // What `old` holds is a valid tree: the copy holds no more than it, so no limit
        // of `push` or `place` is met that `old` did not meet first.
        let _ = old.compact_into(self);
        self.dropped = 0;
    }

    fn compact_into(&mut self, new: &mut Tree) -> Result<(), Error> {
        let mut new_id = vec![NONE; self.nodes.len()];
        let root = self
            .nodes
            .get_mut(Tree::ROOT)
            .map(|n| std::mem::take(&mut n.meta))
            .unwrap_or_default();
        if let Some(r) = new.nodes.get_mut(Tree::ROOT) {
            r.meta = root;
        }
        if let Some(slot) = new_id.get_mut(Tree::ROOT) {
            *slot = 0;
        }
        let mut stack = vec![Tree::ROOT];
        let mut ids = Vec::new();
        while let Some(dir) = stack.pop() {
            let new_dir = new_id.get(dir).copied().unwrap_or(NONE);
            self.link_ids(dir, &mut ids);
            for &link in ids.iter().rev() {
                let Some(l) = self.links.get(link as usize).copied() else {
                    continue;
                };
                let child = &(l.child as NodeId);
                let name = self.names.get(l.name);
                let moved = match new_id.get(*child).copied() {
                    Some(id) if id != NONE => id,
                    _ => {
                        let node = match self.nodes.get_mut(*child) {
                            Some(n) => {
                                // A directory keeps its list here, to be walked when popped.
                                let keep = match n.kind {
                                    Kind::Dir(d) => Kind::Dir(d),
                                    _ => Kind::Fifo,
                                };
                                std::mem::replace(
                                    n,
                                    Node {
                                        kind: keep,
                                        meta: Meta::default(),
                                    },
                                )
                            }
                            None => continue,
                        };
                        let is_dir = matches!(node.kind, Kind::Dir(_));
                        let id = new.new_node(node)?;
                        if let Some(slot) = new_id.get_mut(*child) {
                            *slot = id;
                        }
                        if is_dir {
                            stack.push(*child);
                        }
                        id
                    }
                };
                new.add_link(new_dir, name, moved)?;
            }
        }
        Ok(())
    }

    /// Adds `node` to `dir` as `name`, replacing any entry of that name.
    pub fn insert(&mut self, dir: NodeId, name: &[u8], node: Node) -> Result<NodeId, Error> {
        check_name(name)?;
        self.dir_mut(dir)?;
        let id = self.new_node(node)?;
        self.set(dir, name, id)?;
        Ok(id as NodeId)
    }

    /// Gives `target`, which is not a directory, another name: a hard link.
    pub fn link(&mut self, dir: NodeId, name: &[u8], target: NodeId) -> Result<(), Error> {
        check_name(name)?;
        match self.nodes.get(target).map(|n| &n.kind) {
            None => return err("hard link to a missing node"),
            Some(Kind::Dir(_)) => return err("hard link to a directory"),
            Some(_) => {}
        }
        self.set(dir, name, target as u32)
    }

    /// Moves the entry `name` of `dir` to `to_name` in `to`, replacing any entry there.
    pub fn rename(&mut self, dir: NodeId, name: &[u8], to: NodeId, to_name: &[u8]) -> Result<(), Error> {
        check_name(to_name)?;
        self.dir_mut(to)?;
        let d = u32::try_from(dir).map_err(|_| Error("not a directory".into()))?;
        let Some(id) = self.find(d, name) else {
            return err("no such entry to rename");
        };
        let child = self
            .unset(id)
            .ok_or_else(|| Error("no such entry to rename".into()))?;
        // A move is no drop: the node is still named.
        self.dropped = self.dropped.saturating_sub(1);
        self.set(to, to_name, child as u32)
    }

    /// Removes the entry `name` from `dir`, returning what it named.
    pub fn remove(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        let d = u32::try_from(dir).ok()?;
        self.dir(dir)?;
        let id = self.find(d, name)?;
        self.unset(id)
    }

    /// Removes every entry of `dir`.
    pub fn clear(&mut self, dir: NodeId) {
        let Some(d) = self.dir(dir) else { return };
        let mut at = d.first;
        while let Some(l) = self.links.get(at as usize).copied() {
            if l.child != NONE {
                self.unset(at);
            }
            at = l.next;
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
fn xattr_body(xattrs: &Xattrs, body: &mut Vec<u8>) -> Result<(), Error> {
    body.clear();
    if xattrs.is_empty() {
        return Ok(());
    }
    let mut entries = Vec::with_capacity(xattrs.len());
    for (name, value) in xattrs.iter() {
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
fn xattr_len(xattrs: &Xattrs) -> u64 {
    if xattrs.is_empty() {
        return 0;
    }
    xattrs.iter().fold(XATTR_HEADER, |n, (name, value)| {
        let suffix = xattr_index(name).map_or(name.len(), |(_, rest)| rest.len());
        n + (4 + suffix + value.len()).next_multiple_of(4) as u64
    })
}

/// One inode as laid out: everything the writer decides before writing. Kept small, as
/// an image holds one for each of its files: a directory's entries are made again from
/// the tree when its blocks are written, not held (platform-measurements.md M78), and
/// its xattr body's length from the node when it is needed: 32 bytes.
#[derive(Debug)]
struct Inode {
    node: u32,
    /// For directories: the parent's node.
    parent: u32,
    nlink: u32,
    nid: u32,
    /// First data block, or [`NO_BLOCK`].
    start: u32,
    /// Bytes of data kept in the inode record (FLAT_INLINE), or 0: less than a block.
    tail: u16,
    extended: bool,
    inline: bool,
    size: u64,
}

/// An inode's `start` when it has no data blocks: no block of an image has this number,
/// as the image's block count fits a `u32`.
const NO_BLOCK: u32 = u32::MAX;

impl Inode {
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
}

/// A directory's entries as written into `all`: its names, then `.` and `..`, in byte
/// order, a prefix first, which the kernel's lookup (namei.c) relies on. The tree lists
/// names in that order already, and no name is `.` or `..`.
fn dir_entries<'t>(tree: &'t Tree, id: NodeId, parent: NodeId, all: &mut Vec<Entry<'t>>) {
    tree.entries_into(id, all);
    for (name, to) in [(&b"."[..], id), (&b".."[..], parent)] {
        let at = all.partition_point(|e| e.0 < name);
        all.insert(at, (name, to));
    }
}

/// A directory's blocks, as runs of its entries, given where each block ends.
fn blocks<'e, 't>(all: &'e [Entry<'t>], ends: &'e [usize]) -> impl Iterator<Item = &'e [Entry<'t>]> {
    std::iter::once(0)
        .chain(ends.iter().copied())
        .zip(ends.iter().copied())
        .map(|(from, to)| all.get(from..to).unwrap_or_default())
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
    // name, so the root is first and hard links share one inode. `index` holds each
    // node's place in `order`, by node.
    const NONE: u32 = u32::MAX;
    let too_many = || Error("too many inodes".into());
    let mut order: Vec<Inode> = Vec::with_capacity(tree.len());
    let mut index: Vec<u32> = vec![NONE; tree.len()];
    let mut stack: Vec<(NodeId, NodeId)> = vec![(Tree::ROOT, Tree::ROOT)];
    let mut listed: Vec<Entry<'_>> = Vec::new();
    while let Some((id, parent)) = stack.pop() {
        let slot = index
            .get_mut(id)
            .ok_or_else(|| Error(format!("missing node {id}")))?;
        if let Some(inode) = order.get_mut(*slot as usize) {
            inode.nlink = inode.nlink.saturating_add(1);
            continue;
        }
        *slot = u32::try_from(order.len())
            .ok()
            .filter(|&i| i != NONE)
            .ok_or_else(too_many)?;
        let node = tree.node(id).ok_or_else(|| Error(format!("missing node {id}")))?;
        order.push(Inode {
            node: u32::try_from(id).map_err(|_| too_many())?,
            parent: u32::try_from(parent).map_err(|_| too_many())?,
            nlink: 1,
            nid: 0,
            start: NO_BLOCK,
            tail: 0,
            extended: false,
            inline: false,
            size: 0,
        });
        if matches!(node.kind, Kind::Dir(_)) {
            tree.entries_into(id, &mut listed);
            // Reverse, so the stack yields names in order.
            for &(_, child) in listed.iter().rev() {
                stack.push((child, id));
            }
        }
    }

    // Link counts and sizes. A directory's links are its own `.`, its name, and each
    // subdirectory's `..`.
    let mut epoch = i64::MAX;
    let mut all: Vec<Entry<'_>> = Vec::new();
    for inode in &mut order {
        let id = inode.node as NodeId;
        let node = tree.node(id).ok_or_else(|| Error("missing node".into()))?;
        epoch = epoch.min(node.meta.mtime);
        match &node.kind {
            Kind::Dir(_) => {
                dir_entries(tree, id, inode.parent as NodeId, &mut all);
                // Less `.` and `..`, which are no subdirectories of it.
                let subdirs = all
                    .iter()
                    .filter(|&&(_, c)| matches!(tree.node(c).map(|n| &n.kind), Some(Kind::Dir(_))))
                    .count()
                    .saturating_sub(2);
                inode.nlink = u32::try_from(subdirs)
                    .ok()
                    .and_then(|n| n.checked_add(2))
                    .ok_or_else(too_many)?;
                inode.size = dir_blocks(&all).1;
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
            .node(inode.node as NodeId)
            .ok_or_else(|| Error("missing node".into()))?;
        let m = &node.meta;
        let since_epoch = m.mtime.checked_sub(epoch).and_then(|d| u32::try_from(d).ok());
        inode.extended = m.uid > u32::from(u16::MAX)
            || m.gid > u32::from(u16::MAX)
            || inode.size > u64::from(u32::MAX)
            || inode.nlink > u32::from(u16::MAX)
            || since_epoch.is_none()
            || m.mtime_nsec != 0;
        let xattrs = u32::try_from(xattr_len(&m.xattrs)).map_err(|_| Error("xattrs too large".into()))?;
        let head = inode.isize() + u64::from(xattrs);
        let tail = inode.size % BLOCK;
        let regular = matches!(node.kind, Kind::File { .. });
        inode.inline = !regular && tail > 0 && head + tail <= BLOCK;
        // Less than a block.
        inode.tail = if inode.inline { tail as u16 } else { 0 };
        let record = head + u64::from(inode.tail);
        offset = offset.next_multiple_of(SLOT);
        // A record that fits in a block never crosses one; a larger one starts a block.
        if offset % BLOCK + record > BLOCK {
            offset = offset.next_multiple_of(BLOCK);
        }
        inode.nid = u32::try_from(offset / SLOT).map_err(|_| too_many())?;
        offset += record;
    }
    let root_nid = order.first().map_or(0, |i| i.nid);
    let root_nid = u16::try_from(root_nid).map_err(|_| Error("root inode placed too far".into()))?;
    let meta_blocks = offset.div_ceil(BLOCK);
    let too_large = || Error("image too large for 32-bit block numbers".into());
    let mut next_block = meta_blocks;
    for inode in &mut order {
        let blocks = inode.data_blocks();
        if blocks > 0 {
            inode.start = u32::try_from(next_block).map_err(|_| too_large())?;
            next_block = next_block.checked_add(blocks).ok_or_else(too_large)?;
        }
    }
    let total_blocks = next_block;
    let total = u32::try_from(total_blocks).map_err(|_| too_large())?;
    let nid_of = |id: NodeId| -> u64 {
        index
            .get(id)
            .and_then(|&i| order.get(i as usize))
            .map_or(0, |i| u64::from(i.nid))
    };

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
            .node(inode.node as NodeId)
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
            _ if inode.start == NO_BLOCK => NULL_ADDR,
            _ => inode.start,
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
        seq.pad_to(u64::from(inode.nid) * SLOT)?;
        seq.put(rec.get(..inode.isize() as usize).unwrap_or_default())?;
        seq.put(&xattrs)?;
        if inode.inline {
            let start = inode.size - u64::from(inode.tail);
            let tail = usize::from(inode.tail);
            match &node.kind {
                Kind::Dir(_) => {
                    dir_entries(tree, inode.node as NodeId, inode.parent as NodeId, &mut all);
                    let (ends, _) = dir_blocks(&all);
                    let last = blocks(&all, &ends)
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
        if inode.start == NO_BLOCK {
            continue;
        }
        let node = tree
            .node(inode.node as NodeId)
            .ok_or_else(|| Error("missing node".into()))?;
        let full = inode.data_blocks() * BLOCK;
        let end = seq.at + full;
        match &node.kind {
            Kind::Dir(_) => {
                dir_entries(tree, inode.node as NodeId, inode.parent as NodeId, &mut all);
                let (ends, _) = dir_blocks(&all);
                for (i, entries) in blocks(&all, &ends).enumerate() {
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
            xattrs: Xattrs::default(),
        }
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// A tree is compacted once what was dropped comes to half its nodes, not before: its
    /// version, which compaction makes fresh, tells.
    /// A clone shares every chunk of nodes with its tree until one changes a node, which
    /// copies that node's chunk alone: only its run is unshared after. Trees made apart
    /// share nothing (review 4.2, 4.3).
    #[test]
    fn a_clone_shares_what_it_has_not_changed() {
        let chunk = Arena::<Node>::CHUNK;
        let mut tree = Tree::new(meta(0o755));
        for i in 0..3 * chunk {
            tree.insert(
                Tree::ROOT,
                format!("f{i}").as_bytes(),
                Node {
                    kind: Kind::Fifo,
                    meta: meta(0o644),
                },
            )
            .unwrap();
        }
        let len = tree.len();
        let mut copy = tree.clone();
        assert_eq!(copy.unshared_nodes(&tree, len).count(), 0);
        let changed = chunk + 7;
        copy.node_mut(changed).unwrap().meta.mode = 0o600;
        let runs: Vec<_> = copy.unshared_nodes(&tree, len).collect();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].start, runs[0].end), (chunk, 2 * chunk));
        assert_eq!(
            tree.node(changed).unwrap().meta.mode,
            0o644,
            "the tree keeps its own"
        );
        let apart = Tree::new(meta(0o755));
        assert_eq!(apart.unshared_nodes(&tree, len).count(), len.div_ceil(chunk));
        assert_eq!(
            copy.unshared_nodes(&tree, 10).count(),
            0,
            "the changed chunk lies past the first 10"
        );
    }

    #[test]
    fn compaction_waits_for_half_the_tree_to_be_dropped() {
        let mut tree = Tree::new(meta(0o755));
        let file = || Node {
            kind: Kind::File {
                size: 0,
                data: DataRef { source: 0, offset: 0 },
            },
            meta: meta(0o644),
        };
        for i in 0..100 {
            tree.insert(Tree::ROOT, format!("f{i}").as_bytes(), file())
                .unwrap();
        }
        let version = tree.version();
        for i in 0..40 {
            tree.remove(Tree::ROOT, format!("f{i}").as_bytes());
        }
        tree.compact_if_worth_it();
        assert_eq!(tree.version(), version, "compacted with 40 of 101 nodes dropped");
        for i in 40..60 {
            tree.remove(Tree::ROOT, format!("f{i}").as_bytes());
        }
        tree.compact_if_worth_it();
        assert_ne!(
            tree.version(),
            version,
            "not compacted with 60 of 101 nodes dropped"
        );
        assert_eq!(tree.len(), 41);
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
                    kind: Kind::Dir(Dir::default()),
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
                    kind: Kind::Dir(Dir::default()),
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
                kind: Kind::Symlink(b"etc/f1".to_vec().into()),
                meta: meta(0o777),
            },
        )
        .unwrap();
        tree.insert(
            Tree::ROOT,
            b"long",
            Node {
                kind: Kind::Symlink(vec![b'x'; 5000].into()),
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
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>()
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

    /// The entry index agrees with a map of every directory's names through inserts,
    /// replacements, removals, renames, hard links and clears, seeded so a failure
    /// repeats, across the index's growth and a compaction; and a directory lists its
    /// names sorted and once each.
    #[test]
    fn entries_agree_with_a_model_through_every_change() {
        use std::collections::BTreeMap;
        let mut tree = Tree::new(meta(0o755));
        let dirs: Vec<NodeId> = (0..8)
            .map(|d| {
                let node = Node {
                    kind: Kind::Dir(Dir::default()),
                    meta: meta(0o755),
                };
                tree.insert(Tree::ROOT, format!("d{d}").as_bytes(), node).unwrap()
            })
            .collect();
        let mut model: Vec<BTreeMap<Vec<u8>, NodeId>> = vec![BTreeMap::new(); dirs.len()];
        let file = || Node {
            kind: Kind::Fifo,
            meta: meta(0o644),
        };
        // xorshift64*: deterministic, no dependency.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |n: u64| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        };
        for _ in 0..60_000 {
            let d = next(dirs.len() as u64) as usize;
            let name = format!("n{}", next(3_000)).into_bytes();
            match next(10) {
                0..=4 => {
                    let id = tree.insert(dirs[d], &name, file()).unwrap();
                    model[d].insert(name, id);
                }
                5 | 6 => {
                    assert_eq!(tree.remove(dirs[d], &name), model[d].remove(&name));
                }
                7 => {
                    let to = next(dirs.len() as u64) as usize;
                    let to_name = format!("n{}", next(3_000)).into_bytes();
                    let r = tree.rename(dirs[d], &name, dirs[to], &to_name);
                    match model[d].remove(&name) {
                        Some(id) => {
                            r.unwrap();
                            model[to].insert(to_name, id);
                        }
                        None => assert!(r.is_err()),
                    }
                }
                8 => {
                    if let Some((_, &id)) = model[d].iter().next() {
                        tree.link(dirs[d], &name, id).unwrap();
                        model[d].insert(name, id);
                    }
                }
                _ => {
                    if next(50) == 0 {
                        tree.clear(dirs[d]);
                        model[d].clear();
                    }
                }
            }
        }
        let check = |tree: &Tree, model: &[BTreeMap<Vec<u8>, NodeId>], same_ids: bool| {
            for (d, m) in model.iter().enumerate() {
                let dir = tree.child(Tree::ROOT, format!("d{d}").as_bytes()).unwrap();
                let listed = tree.entries(dir);
                assert_eq!(tree.dir_len(dir), Some(m.len()));
                assert_eq!(
                    listed.iter().map(|(n, _)| n.to_vec()).collect::<Vec<_>>(),
                    m.keys().cloned().collect::<Vec<_>>()
                );
                for (name, &id) in m {
                    let found = tree.child(dir, name).unwrap();
                    if same_ids {
                        assert_eq!(found, id);
                    }
                }
                assert_eq!(tree.child(dir, b"never"), None);
            }
        };
        check(&tree, &model, true);
        tree.compact();
        check(&tree, &model, false);
    }

    /// Compacting drops only what the root no longer reaches: the image is the same
    /// bytes, the arena holds its reachable nodes alone, and a node that loses one of its
    /// two names keeps the other (audit D11).
    #[test]
    fn compaction_drops_history_and_keeps_the_image() {
        let (mut tree, mut mem) = sample();
        let reachable = tree.len();
        // Nothing replaced or removed: nothing to do, and ids still hold.
        let etc = tree.child(Tree::ROOT, b"etc").unwrap();
        tree.compact();
        assert_eq!(
            (tree.len(), tree.child(Tree::ROOT, b"etc")),
            (reachable, Some(etc))
        );
        // Replaced four times over, and a file removed.
        for generation in 0..4u8 {
            let node = Node {
                kind: Kind::Symlink(vec![b'a' + generation; 100].into()),
                meta: meta(0o777),
            };
            tree.insert(Tree::ROOT, b"again", node).unwrap();
        }
        let before = image(&tree, &mut mem);
        assert_eq!(tree.len(), reachable + 4);
        tree.compact();
        // Four symlinks in, three of them replaced.
        assert_eq!(tree.len(), reachable + 1);
        assert_eq!(image(&tree, &mut mem), before, "compaction changed the image");
        // Ids are renumbered: `etc` is found again.
        let etc = tree.child(Tree::ROOT, b"etc").unwrap();
        tree.remove(etc, b"f1").unwrap();
        tree.compact();
        assert_eq!(tree.len(), reachable);
        // The hard link's other name keeps its node.
        let etc = tree.child(Tree::ROOT, b"etc").unwrap();
        tree.remove(etc, b"f4097").unwrap();
        tree.compact();
        let after = image(&tree, &mut mem);
        let r = Reader::open(&after);
        let hard = r.lookup("hardlink");
        assert_eq!((hard.nlink, r.data(&hard).len()), (1, 4097));
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
                    kind: Kind::Dir(Dir::default()),
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
