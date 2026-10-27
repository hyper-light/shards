//! A builder's caches (docs/design/architecture.md D134): each the layers of a cache
//! record, EROFS images the host attaches to the builder's join disk, stacked by overlayfs
//! under an upper directory of the step's; and the step's changes, that upper directory,
//! written back to a range of the join disk the host attached for them, as an EROFS image
//! whose length the host is told. Neither crosses the build's connection.
//!
//! Each layer is mounted once for the builder's life, from a read-only loop device over its
//! range: the join disk never gives a range twice (shards_vmm `block::Join`), so what the
//! guest has cached of one is never another's.
//!
//! A file of several names stays one file, as a directory keeps it: overlayfs's inode index
//! (`index=on`) copies one up once, under every name it is reached by, and at the step's end
//! the names its changes did not reach are written into its image as links of what was
//! copied up, where overlayfs alone would leave them the old file below (copy_up.c: no
//! origin, "breaking the association with a lower hard link"). The index tells layers
//! apart by their volumes' UUIDs, which each image has of its own.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use shards_abi::build::{Cache, Kept, Root};
use shards_erofs::{DataRef, Dir, Kind, Meta, Node, NodeId, Source, Tree};

use crate::build::{cstr, layer_name, mount, os_err, umount};
use crate::join::{
    LO_FLAGS_AUTOCLEAR, LO_FLAGS_READ_ONLY, LOOP_CTL_GET_FREE, LOOP_SET_FD, LOOP_SET_STATUS64, LoopInfo64,
    direct_io,
};
use crate::tree::{OPAQUE, getxattr, xattrs};

/// Where the layers are mounted, each under a short name, so that a stack of them fits the
/// one page of a mount's options.
pub const LAYERS: &str = "/k";

/// How much of an image goes to the join disk at once.
const WRITE_CHUNK: usize = 1 << 20;

fn err(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

/// Where in an EROFS image its volume's UUID is: the superblock's, 1024 bytes in, at 48
/// (erofs_fs.h, `erofs_super_block.uuid`).
const UUID_AT: u64 = 1024 + 48;

/// A layer mounted: its mount's name under [`LAYERS`], and its volume's UUID.
#[derive(Debug, Clone)]
struct Layer {
    name: String,
    uuid: [u8; 16],
}

/// The builder's caches: its join disk once a step needs it, and the layers mounted.
#[derive(Default)]
pub struct Caches {
    /// The join disk's name under /sys/block, and its device, open to read and write.
    disk: Option<(String, File)>,
    /// Each layer mounted, by where its range starts.
    mounted: HashMap<u64, Layer>,
    next: u32,
}

impl Caches {
    /// The join disk, found by its serial, which the builder's sysfs holds.
    fn disk(&mut self) -> io::Result<(&str, &File)> {
        if self.disk.is_none() {
            let (name, _, _) = crate::join::join_disk().map_err(err)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(format!("/dev/{name}"))?;
            self.disk = Some((name, file));
        }
        let (name, file) = self.disk.as_ref().ok_or_else(|| err("no join disk"))?;
        Ok((name, file))
    }

    /// Waits until the join disk holds `end` bytes: the host attached what the step needs
    /// before it sent the step, and the driver reads the capacity it raised on a workqueue.
    fn reach(&mut self, end: u64) -> io::Result<()> {
        let (name, _) = self.disk()?;
        crate::join::await_capacity(name, end).map_err(err)
    }

    /// The layer at `start`, `len` bytes of the join disk: mounted, if it is not yet.
    fn layer(&mut self, start: u64, len: u64) -> io::Result<Layer> {
        if let Some(layer) = self.mounted.get(&start) {
            return Ok(layer.clone());
        }
        self.reach(
            start
                .checked_add(len)
                .ok_or_else(|| err("a layer past the join disk"))?,
        )?;
        let (_, disk) = self.disk()?;
        let mut uuid = [0u8; 16];
        disk.read_exact_at(
            &mut uuid,
            start
                .checked_add(UUID_AT)
                .ok_or_else(|| err("a layer past the join disk"))?,
        )?;
        let (device, held) = loop_device(disk, start, len)?;
        let name = layer_name(self.next);
        self.next = self.next.wrapping_add(1);
        let at = Path::new(LAYERS).join(&name);
        std::fs::create_dir_all(&at)?;
        mount(&device, &at, "erofs", libc::MS_RDONLY, "")?;
        // The mount holds the device now, which lets go of its range with it.
        drop(held);
        let layer = Layer { name, uuid };
        self.mounted.insert(start, layer.clone());
        Ok(layer)
    }

    /// The layers of `cache`, the lowest first, each mounted.
    fn layers(&mut self, cache: &Cache) -> io::Result<Vec<Layer>> {
        cache
            .layers
            .iter()
            .map(|&(start, len)| self.layer(start, len))
            .collect()
    }

    /// Mounts `cache` at `at` for a step: its layers under `upper`, a fresh directory with
    /// the root the record was left with, as overlayfs shows its upper directory's, and
    /// containerd gives a snapshot its parent's owner. `empty` stands under a cache with no
    /// layers, as overlayfs takes one lower directory at least.
    pub fn mount(&mut self, cache: &Cache, at: &Path, upper: &Path, empty: &str) -> io::Result<()> {
        std::fs::create_dir_all(upper)?;
        let p = cstr(upper)?;
        // SAFETY: a NUL-terminated path.
        unsafe {
            if libc::chown(p.as_ptr(), cache.root.uid, cache.root.gid) != 0
                || libc::chmod(p.as_ptr(), cache.root.mode & 0o7777) != 0
            {
                return Err(os_err("a cache's root"));
            }
        }
        let work = upper.with_extension("work");
        std::fs::create_dir_all(&work)?;
        let mut lower: Vec<String> = self.layers(cache)?.into_iter().rev().map(|l| l.name).collect();
        if lower.is_empty() {
            lower.push(empty.to_string());
        }
        // The inode index, so that a file of several names stays one (the module's doc); no
        // redirects, no metacopy, as a layer holds what was written; its upper directory is
        // never needed after a crash.
        let data = format!(
            "lowerdir={},upperdir={},workdir={},index=on,redirect_dir=off,metacopy=off,volatile,{}",
            lower.join(":"),
            upper.display(),
            work.display(),
            crate::run::XINO
        );
        if data.len() >= 4096 {
            return Err(err("too many layers for one mount"));
        }
        std::fs::create_dir_all(at)?;
        std::env::set_current_dir(LAYERS)?;
        let mounted = mount("overlay", at, "overlay", 0, &data);
        std::env::set_current_dir("/")?;
        mounted
    }

    /// What a step's changes to `cache`, its upper directory `upper` (unmounted now), came
    /// to: the root they left it, and, where they are kept, the image of them written to
    /// its `out` range, with what they hide of its layers, seen through a mount at
    /// `lower_at`. A step never set up leaves the root as it was given.
    pub fn kept(&mut self, cache: &Cache, upper: &Path, lower_at: &Path) -> Kept {
        let root = match std::fs::symlink_metadata(upper) {
            Ok(m) => Root {
                mode: m.mode() & 0o7777,
                uid: m.uid(),
                gid: m.gid(),
            },
            Err(_) => {
                return Kept {
                    root: cache.root,
                    ..Kept::default()
                };
            }
        };
        let Some((start, len)) = cache.out else {
            return Kept {
                root,
                ..Kept::default()
            };
        };
        match self.write(cache, start, len, upper, lower_at) {
            Ok((len, hidden)) => Kept {
                root,
                len,
                hidden,
                failed: Vec::new(),
            },
            Err(e) => Kept {
                root,
                failed: format!("writing a cache's changes: {e}").into_bytes(),
                ..Kept::default()
            },
        }
    }

    /// Writes the image of `upper` to the `len` bytes of the join disk from `start`, and
    /// syncs it, so that the host reads what was written: its length, 0 where nothing
    /// changed, and the bytes it hides.
    fn write(
        &mut self,
        cache: &Cache,
        start: u64,
        len: u64,
        upper: &Path,
        lower_at: &Path,
    ) -> io::Result<(u64, u64)> {
        let (mut tree, files, copied) = upper_tree(upper)?;
        if tree.dir_len(Tree::ROOT).unwrap_or(0) == 0 {
            return Ok((0, 0));
        }
        let layers = self.layers(cache)?;
        let hidden = match View::of(&layers, lower_at)? {
            Some(view) => {
                let hidden = hidden_under(&tree, view.path())?;
                complete_links(&mut tree, &copied, &layers, view.path())?;
                hidden
            }
            None => 0,
        };
        self.reach(
            start
                .checked_add(len)
                .ok_or_else(|| err("changes past the join disk"))?,
        )?;
        let (_, disk) = self.disk()?;
        let mut out = BufWriter::with_capacity(
            WRITE_CHUNK,
            Range {
                disk,
                start,
                len,
                at: 0,
            },
        );
        // A volume of its own, as overlayfs's index needs of each layer.
        let mut uuid = [0u8; 16];
        // SAFETY: getrandom(2) into a buffer of the length given.
        if unsafe { libc::getrandom(uuid.as_mut_ptr().cast(), uuid.len(), 0) } != 16 {
            return Err(os_err("a layer's UUID"));
        }
        shards_erofs::write_uuid(
            &tree,
            &mut Files {
                paths: files,
                open: None,
            },
            &mut out,
            uuid,
        )
        .map_err(|e| err(e.0))?;
        let range = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        disk.sync_data()?;
        // The host reads them from here on: the guest needs none of their pages.
        // SAFETY: posix_fadvise(2) on a descriptor this process holds.
        unsafe {
            libc::posix_fadvise(
                disk.as_raw_fd(),
                i64::try_from(start).unwrap_or(i64::MAX),
                i64::try_from(range.at).unwrap_or(0),
                libc::POSIX_FADV_DONTNEED,
            );
        }
        Ok((range.at, hidden))
    }
}

/// What a step saw of its cache's layers: a read-only overlay of them at a mount of its
/// own while it is held, or the one layer's own mount.
struct View {
    path: PathBuf,
    mounted: bool,
}

impl View {
    /// `layers`' view, the lowest first: mounted at `at` where there are several, none
    /// where there are none.
    fn of(layers: &[Layer], at: &Path) -> io::Result<Option<View>> {
        match layers {
            [] => Ok(None),
            [one] => Ok(Some(View {
                path: Path::new(LAYERS).join(&one.name),
                mounted: false,
            })),
            _ => {
                let lower: Vec<&str> = layers.iter().rev().map(|l| l.name.as_str()).collect();
                std::fs::create_dir_all(at)?;
                std::env::set_current_dir(LAYERS)?;
                let mounted = mount(
                    "overlay",
                    at,
                    "overlay",
                    libc::MS_RDONLY,
                    &format!(
                        "lowerdir={},redirect_dir=off,metacopy=off,{}",
                        lower.join(":"),
                        crate::run::XINO
                    ),
                );
                std::env::set_current_dir("/")?;
                mounted?;
                Ok(Some(View {
                    path: at.to_path_buf(),
                    mounted: true,
                }))
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for View {
    fn drop(&mut self) {
        if self.mounted {
            let _ = umount(&self.path);
        }
    }
}

/// A loop device over `len` bytes of `disk` from `offset`, read-only, and let go with its
/// last user (`LO_FLAGS_AUTOCLEAR`): its path, and the device open, as it lets go of its
/// range at its last close, so it is held until a mount holds it. With LOOP_SET_FD and
/// LOOP_SET_STATUS64, which every kernel has, as a joiner's root is made (join.rs).
fn loop_device(disk: &File, offset: u64, len: u64) -> io::Result<(String, File)> {
    let control = File::open("/dev/loop-control")?;
    for _ in 0..64 {
        // SAFETY: ioctl(2) on loop-control, which takes no argument.
        let n = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
        if n < 0 {
            return Err(os_err("a free loop device"));
        }
        let path = format!("/dev/loop{n}");
        // Opened read-only, the device is read-only (loop_configure).
        let dev = File::open(&path)?;
        // SAFETY: ioctl(2) on the loop device with the backing disk's descriptor.
        if unsafe { libc::ioctl(dev.as_raw_fd(), LOOP_SET_FD as _, disk.as_raw_fd()) } != 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) {
                continue;
            }
            return Err(os_err("a loop device over the join disk"));
        }
        let info = LoopInfo64 {
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: offset,
            lo_sizelimit: len,
            lo_number: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR,
            lo_file_name: [0; 64],
            lo_crypt_name: [0; 64],
            lo_encrypt_key: [0; 32],
            lo_init: [0; 2],
        };
        // SAFETY: ioctl(2) reading `info`, which outlives it.
        if unsafe { libc::ioctl(dev.as_raw_fd(), LOOP_SET_STATUS64 as _, &raw const info) } != 0 {
            return Err(os_err("a layer's range on its loop device"));
        }
        // Read past the join disk's own cache (join.rs `direct_io`).
        direct_io(&dev).map_err(|_| os_err("a layer's range read past the join disk's cache"))?;
        return Ok((path, dev));
    }
    Err(err("no loop device stayed free"))
}

/// The writable range of the join disk a cache's changes go to, written in order, never
/// past its end.
struct Range<'d> {
    disk: &'d File,
    start: u64,
    len: u64,
    /// How much has been written.
    at: u64,
}

impl Write for Range<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self
            .at
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.len)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!("they take more than the {} bytes the host had room for", self.len),
                )
            })?;
        let offset = self
            .start
            .checked_add(self.at)
            .ok_or_else(|| err("a write past the join disk"))?;
        self.disk.write_all_at(buf, offset)?;
        self.at = end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The regular files of an upper directory, by the index their nodes' data names: each
/// opened as its bytes are read, never through a symlink, the last one kept open.
struct Files {
    paths: Vec<PathBuf>,
    open: Option<(u32, File)>,
}

impl Source for Files {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let source = data.source;
        if self.open.as_ref().is_none_or(|(s, _)| *s != source) {
            let path = usize::try_from(source)
                .ok()
                .and_then(|i| self.paths.get(i))
                .ok_or_else(|| err("a file of no path"))?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            self.open = Some((source, file));
        }
        let Some((_, file)) = &self.open else {
            return Err(err("a file not open"));
        };
        let offset = data
            .offset
            .checked_add(at)
            .ok_or_else(|| err("a read past a file"))?;
        file.read_exact_at(buf, offset)
    }
}

/// A regular file of an upper directory that overlayfs's index copied up, as it copies up a
/// file of several names (`ovl_need_index`): its node, its links (the index's among them),
/// and its origin, the file handle of what it was copied up from.
struct Copied {
    node: NodeId,
    nlink: u64,
    origin: Vec<u8>,
}

/// overlayfs's xattr naming what an upper file was copied up from (overlayfs.h).
const ORIGIN: &[u8] = b"trusted.overlay.origin";

/// An upper directory as a tree, as overlayfs left it: each whiteout a character device
/// numbered 0:0, each opaque directory marked as overlayfs marks one, overlayfs's other
/// xattrs, its own bookkeeping, gone; each file of several names one node. Its regular
/// files' paths, by the index their data names, and those overlayfs's index copied up.
fn upper_tree(upper: &Path) -> io::Result<(Tree, Vec<PathBuf>, Vec<Copied>)> {
    let m = std::fs::symlink_metadata(upper)?;
    let mut tree = Tree::new(meta_of(&m, &cstr(upper)?, true)?);
    let mut files = Vec::new();
    let mut copied = Vec::new();
    let mut linked: HashMap<(u64, u64), NodeId> = HashMap::new();
    let mut todo = vec![(upper.to_path_buf(), Tree::ROOT)];
    while let Some((dir, at)) = todo.pop() {
        for child in std::fs::read_dir(&dir)? {
            let child = child?;
            let name = child.file_name();
            let path = child.path();
            let m = std::fs::symlink_metadata(&path)?;
            let t = m.file_type();
            if !t.is_dir()
                && m.nlink() > 1
                && let Some(&node) = linked.get(&(m.dev(), m.ino()))
            {
                tree.link(at, name.as_bytes(), node).map_err(|e| err(e.0))?;
                continue;
            }
            let rdev = m.rdev();
            let whiteout = t.is_char_device() && rdev == 0;
            let meta = meta_of(&m, &cstr(&path)?, !whiteout)?;
            let kind = if whiteout {
                Kind::CharDevice { major: 0, minor: 0 }
            } else if t.is_dir() {
                Kind::Dir(Dir::default())
            } else if t.is_file() {
                let source = u32::try_from(files.len()).map_err(|_| err("too many files"))?;
                files.push(path.clone());
                Kind::File {
                    size: m.size(),
                    data: DataRef { source, offset: 0 },
                }
            } else if t.is_symlink() {
                Kind::Symlink(std::fs::read_link(&path)?.as_os_str().as_bytes().into())
            } else if t.is_char_device() {
                Kind::CharDevice {
                    major: libc::major(rdev),
                    minor: libc::minor(rdev),
                }
            } else if t.is_block_device() {
                Kind::BlockDevice {
                    major: libc::major(rdev),
                    minor: libc::minor(rdev),
                }
            } else if t.is_fifo() {
                Kind::Fifo
            } else {
                Kind::Socket
            };
            let is_dir = matches!(kind, Kind::Dir(_));
            let node = tree
                .insert(at, name.as_bytes(), Node { kind, meta })
                .map_err(|e| err(e.0))?;
            if is_dir {
                todo.push((path, node));
            } else if m.nlink() > 1 {
                linked.insert((m.dev(), m.ino()), node);
                // The index's link is one of them, where it copied the file up.
                if t.is_file()
                    && let Some(origin) = getxattr(&cstr(&path)?, ORIGIN)?
                {
                    copied.push(Copied {
                        node,
                        nlink: m.nlink(),
                        origin,
                    });
                }
            }
        }
    }
    Ok((tree, files, copied))
}

/// The layer and inode an origin names, as overlayfs encodes one (overlayfs.h, `ovl_fb`:
/// version 0, magic 0xfb, its length, flags, the fid's type, the volume's UUID, the fid) of
/// an EROFS inode (super.c, `erofs_encode_fh`: the nid's high and low halves, then its
/// generation, each a u32 as the kernel holds it).
fn origin_of(fh: &[u8]) -> Option<([u8; 16], u64)> {
    const FILEID_INO64_GEN: u8 = 0x81;
    const FILEID_INO64_GEN_PARENT: u8 = 0x82;
    let (&[0, 0xfb, len, _flags, kind], rest) = fh.split_first_chunk::<5>()? else {
        return None;
    };
    if usize::from(len) != fh.len() || !matches!(kind, FILEID_INO64_GEN | FILEID_INO64_GEN_PARENT) {
        return None;
    }
    let (uuid, fid) = rest.split_first_chunk::<16>()?;
    let (hi, fid) = fid.split_first_chunk::<4>()?;
    let (lo, _) = fid.split_first_chunk::<4>()?;
    let nid = (u64::from(u32::from_ne_bytes(*hi)) << 32) | u64::from(u32::from_ne_bytes(*lo));
    Some((*uuid, nid))
}

/// The names below of the files overlayfs's index copied up that the step's changes did not
/// reach, as links of what was copied up: a directory keeps a file of several names one
/// file, and a layer below names the old one. Each such file's names are found in the layer
/// its origin is in, by their inode, and kept where nothing above them hides them, the
/// changes' own entries included; `view` is what the step saw below its changes, whose
/// directories a name's missing parents take.
fn complete_links(tree: &mut Tree, copied: &[Copied], layers: &[Layer], view: &Path) -> io::Result<()> {
    if copied.is_empty() {
        return Ok(());
    }
    // How many names the changes give each copied-up file: fewer than its links, and a
    // name below is not among them.
    let mut names: HashMap<NodeId, u64> = HashMap::new();
    let mut todo = vec![Tree::ROOT];
    while let Some(dir) = todo.pop() {
        for (_, id) in tree.entries(dir) {
            match tree.node(id).map(|n| &n.kind) {
                Some(Kind::Dir(_)) => todo.push(id),
                Some(Kind::File { .. }) => *names.entry(id).or_default() += 1,
                _ => {}
            }
        }
    }
    // By layer, the inodes wanted there and what each was copied up to.
    let mut wanted: HashMap<usize, HashMap<u64, NodeId>> = HashMap::new();
    for c in copied {
        // One the index copied up has a link more than its names, the index's: it was a
        // file of several names below. One copied up alone, or made by the step, has none.
        if c.nlink <= names.get(&c.node).copied().unwrap_or(0) {
            continue;
        }
        let Some((uuid, nid)) = origin_of(&c.origin) else {
            continue;
        };
        if let Some(at) = layers.iter().position(|l| l.uuid == uuid) {
            wanted.entry(at).or_default().insert(nid, c.node);
        }
    }
    for (at, nids) in wanted {
        let Some(layer) = layers.get(at) else {
            continue;
        };
        let root = Path::new(LAYERS).join(&layer.name);
        let above: Vec<PathBuf> = layers
            .get(at + 1..)
            .unwrap_or_default()
            .iter()
            .map(|l| Path::new(LAYERS).join(&l.name))
            .collect();
        for (rel, node) in names_of(&root, &nids)? {
            let mut hidden = tree_hides(tree, &rel);
            for a in &above {
                if hidden {
                    break;
                }
                hidden = layer_hides(a, &rel)?;
            }
            if !hidden {
                link_at(tree, &rel, node, view)?;
            }
        }
    }
    Ok(())
}

/// The regular files under `root`, a layer's own mount, whose inode is among `nids` (an
/// EROFS inode's number is its nid, with no metadata compressed, inode.c), each by its
/// path from `root`, with what `nids` says of it.
fn names_of(root: &Path, nids: &HashMap<u64, NodeId>) -> io::Result<Vec<(PathBuf, NodeId)>> {
    let mut out = Vec::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        for child in std::fs::read_dir(root.join(&rel))? {
            let child = child?;
            let m = std::fs::symlink_metadata(child.path())?;
            let path = rel.join(child.file_name());
            if m.is_dir() {
                todo.push(path);
            } else if m.is_file()
                && m.nlink() > 1
                && let Some(&node) = nids.get(&m.ino())
            {
                out.push((path, node));
            }
        }
    }
    Ok(out)
}

/// Whether the changes in `tree` hide `rel` below them: an entry of theirs there, or a
/// directory above it that is no directory, or opaque.
fn tree_hides(tree: &Tree, rel: &Path) -> bool {
    let mut at = Tree::ROOT;
    let parts: Vec<&OsStr> = rel.iter().collect();
    for (i, part) in parts.iter().enumerate() {
        let Some(id) = tree.child(at, part.as_bytes()) else {
            return false;
        };
        let last = i + 1 == parts.len();
        match tree.node(id) {
            Some(_) if last => return true,
            Some(Node {
                kind: Kind::Dir(_),
                meta,
            }) if meta.xattrs.get(OPAQUE).is_none_or(|v| v != b"y") => at = id,
            _ => return true,
        }
    }
    false
}

/// Whether a layer above, its own mount at `root`, hides `rel` below it: an entry there,
/// or a directory above it that is no directory (a whiteout among them), or opaque.
fn layer_hides(root: &Path, rel: &Path) -> io::Result<bool> {
    let mut at = root.to_path_buf();
    let parts: Vec<&OsStr> = rel.iter().collect();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        let m = match std::fs::symlink_metadata(&at) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if i + 1 == parts.len() || !m.is_dir() {
            return Ok(true);
        }
        if getxattr(&cstr(&at)?, OPAQUE)?.as_deref() == Some(b"y") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `node` named `rel` in `tree` too, each directory above it the changes lack made as the
/// step saw it in `view`, opaque never, as it merges with what is below.
fn link_at(tree: &mut Tree, rel: &Path, node: NodeId, view: &Path) -> io::Result<()> {
    let parts: Vec<&OsStr> = rel.iter().collect();
    let Some((name, dirs)) = parts.split_last() else {
        return Ok(());
    };
    let mut at = Tree::ROOT;
    let mut seen = view.to_path_buf();
    for part in dirs {
        seen.push(part);
        at = match tree.child(at, part.as_bytes()) {
            Some(id) => id,
            None => {
                let m = std::fs::symlink_metadata(&seen)?;
                let mut meta = meta_of(&m, &cstr(&seen)?, true)?;
                meta.xattrs.remove(OPAQUE);
                tree.insert(
                    at,
                    part.as_bytes(),
                    Node {
                        kind: Kind::Dir(Dir::default()),
                        meta,
                    },
                )
                .map_err(|e| err(e.0))?
            }
        };
    }
    tree.link(at, name.as_bytes(), node).map_err(|e| err(e.0))
}

/// A node's metadata from its `lstat` and, where `with_xattrs`, its xattrs but
/// overlayfs's own, and its opacity, as overlayfs marks it.
fn meta_of(m: &std::fs::Metadata, path: &CString, with_xattrs: bool) -> io::Result<Meta> {
    let mut meta = Meta {
        mode: (m.mode() & 0o7777) as u16,
        uid: m.uid(),
        gid: m.gid(),
        mtime: m.mtime(),
        mtime_nsec: u32::try_from(m.mtime_nsec()).unwrap_or(0),
        xattrs: shards_erofs::Xattrs::default(),
    };
    if with_xattrs {
        meta.xattrs = xattrs(path)?.into_iter().collect();
        if m.is_dir() && getxattr(path, OPAQUE)?.as_deref() == Some(b"y") {
            meta.xattrs.insert(OPAQUE.to_vec(), b"y".to_vec());
        }
    }
    Ok(meta)
}

/// The bytes of the regular files at `view` that `tree` hides, each a whole number of
/// blocks: under each of its names, whatever `view` holds there, but a directory not
/// opaque over a directory, which merge, and are looked into.
fn hidden_under(tree: &Tree, view: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    let mut todo = vec![(Tree::ROOT, view.to_path_buf())];
    while let Some((dir, path)) = todo.pop() {
        for (name, id) in tree.entries(dir) {
            let below = path.join(OsStr::from_bytes(name));
            let m = match std::fs::symlink_metadata(&below) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let merges = tree.node(id).is_some_and(|n| {
                matches!(n.kind, Kind::Dir(_)) && n.meta.xattrs.get(OPAQUE).is_none_or(|v| v != b"y")
            }) && m.is_dir();
            if merges {
                todo.push((id, below));
            } else {
                total = total.saturating_add(bytes_under(&below, &m)?);
            }
        }
    }
    Ok(total)
}

/// The bytes of the regular files at `path` and under it, each a whole number of blocks;
/// no symlink followed.
fn bytes_under(path: &Path, m: &std::fs::Metadata) -> io::Result<u64> {
    let block = shards_erofs::BLOCK;
    if !m.is_dir() {
        return Ok(if m.is_file() {
            m.size().next_multiple_of(block)
        } else {
            0
        });
    }
    let mut total = 0u64;
    let mut todo = vec![path.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for child in std::fs::read_dir(&dir)? {
            let child = child?;
            let m = std::fs::symlink_metadata(child.path())?;
            if m.is_dir() {
                todo.push(child.path());
            } else if m.is_file() {
                total = total.saturating_add(m.size().next_multiple_of(block));
            }
        }
    }
    Ok(total)
}
