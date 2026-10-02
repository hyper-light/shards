//! A snapshot's files in memory, changed as Linux changes a file system, so BuildKit's
//! file operations, written against a mounted snapshot, port to it call for call
//! (docs/design/architecture.md D33).
//!
//! What it holds to (Linux 6.x, fs/namei.c, fs/inode.c, fs/attr.c, fs/open.c):
//! - Paths resolve as namei resolves them: symlinks in the middle of a path are always
//!   followed, the last one only when asked, at most 40 of them; an absolute target
//!   starts again at the snapshot's root, which `..` never leaves. A mounted snapshot
//!   resolves an absolute target against the host's root instead: here nothing escapes.
//! - Creating, removing or renaming an entry stamps its directory's mtime with `now`, as
//!   writing a file's data stamps the file's.
//! - New entries are owned by root, BuildKit's daemon, or take the group of a set-group-ID
//!   directory, and new directories inherit its set-group-ID bit (inode_init_owner).
//!   The mode asked for loses the umask, 022 as dockerd sets it; mkdir drops the set-ID
//!   bits; symlinks are 0777.
//! - chown on anything but a directory clears set-user-ID, set-group-ID when the group
//!   may execute, and file capabilities, even when the owner stays (chown_common).
//! - Extended attributes go only where Linux takes them: `user.*` on files and
//!   directories alone, overlayfs's own `trusted.overlay.*` nowhere, and only the
//!   `user`, `trusted`, `security` and `system.posix_acl_*` namespaces.

use std::collections::BTreeSet;
use std::fmt;

use shards_dockerfile::go;
use shards_image::erofs::{DataRef, Dir, EntryId, Kind, Meta, Node, NodeId, Tree};

/// MAXSYMLINKS: how many symlinks one path walk follows.
const MAX_LINKS: u32 = 40;
/// NAME_MAX.
const NAME_MAX: usize = 255;
/// The umask dockerd runs BuildKit with (cmd/dockerd setDefaultUmask).
pub const UMASK: u32 = 0o022;
pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_ISVTX: u32 = 0o1000;
const S_IXGRP: u32 = 0o010;
pub const CAPABILITY: &[u8] = b"security.capability";

/// Errors as Go's syscall.Errno prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    Perm,
    NoEnt,
    Exist,
    NotDir,
    IsDir,
    Inval,
    NameTooLong,
    Loop,
    NotEmpty,
    NotSup,
    Busy,
    XDev,
}

impl Errno {
    pub fn text(self) -> &'static str {
        match self {
            Errno::Perm => "operation not permitted",
            Errno::NoEnt => "no such file or directory",
            Errno::Exist => "file exists",
            Errno::NotDir => "not a directory",
            Errno::IsDir => "is a directory",
            Errno::Inval => "invalid argument",
            Errno::NameTooLong => "file name too long",
            Errno::Loop => "too many levels of symbolic links",
            Errno::NotEmpty => "directory not empty",
            Errno::NotSup => "operation not supported",
            Errno::Busy => "device or resource busy",
            Errno::XDev => "invalid cross-device link",
        }
    }
}

/// Go's `*os.PathError`: `op path: err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    pub op: &'static str,
    pub path: Vec<u8>,
    pub errno: Errno,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.op,
            String::from_utf8_lossy(&self.path),
            self.errno.text()
        )
    }
}

fn fail<T>(op: &'static str, path: &[u8], errno: Errno) -> Result<T, PathError> {
    Err(PathError {
        op,
        path: path.to_vec(),
        errno,
    })
}

/// What a path names, once the directories before its last element are found.
enum Found {
    /// Its node, and the entry naming it, which the root has none of: where overlayfs
    /// would copy it up to.
    Node(NodeId, Option<EntryId>),
    /// The last element is not there; its directory is.
    Missing { dir: NodeId, name: Vec<u8> },
}

/// Where a path leads, its last symlink not followed: what each call on that path would
/// find, resolved once, so that what follows on the same path acts on it directly.
#[derive(Debug)]
pub enum Place {
    /// Something is there: its node, and the entry naming it (none for the root).
    Is(NodeId, Option<EntryId>),
    /// Nothing is there: the directory that would hold it, and the name.
    Free(NodeId, Vec<u8>),
}

/// What one step changed, as overlayfs's upper directory records it over the snapshot
/// the step started from. Every path made, changed or removed, with the directories above
/// it, is stamped in the tree with the step ([`Tree::mark`]); what is kept here, by path,
/// is what was removed, and the directories made where one had been, which overlayfs
/// makes opaque when the snapshot below has the path.
#[derive(Debug, Clone, Default)]
pub struct Upper {
    removed: BTreeSet<Vec<u8>>,
    pub recreated: BTreeSet<Vec<u8>>,
}

/// A file's type, from its kind, as the `S_IFMT` bits of `st_mode`.
pub fn type_bits(kind: &Kind) -> u32 {
    match kind {
        Kind::Dir(_) => 0o040_000,
        Kind::File { .. } => 0o100_000,
        Kind::Symlink(_) => 0o120_000,
        Kind::CharDevice { .. } => 0o020_000,
        Kind::BlockDevice { .. } => 0o060_000,
        Kind::Fifo => 0o010_000,
        Kind::Socket => 0o140_000,
    }
}

/// A snapshot's tree, the time the kernel would stamp changes with, and what the step
/// writing it has changed.
#[derive(Debug, Clone)]
pub struct Fs {
    /// Changed only through the methods below, which record each change as the step's
    /// ([`Tree::mark`]): what a step changes cannot go unrecorded.
    tree: Tree,
    /// Seconds and nanoseconds since 1970.
    pub now: (i64, u32),
    pub upper: Upper,
    /// Where paths resolve from: the snapshot's root, or a directory [`Fs::chroot`] made
    /// the root.
    root: NodeId,
}

impl Fs {
    /// A snapshot of `tree`, recording what changes from now on as one step, as
    /// [`Fs::begin`] starts one.
    pub fn new(mut tree: Tree, now: (i64, u32)) -> Fs {
        tree.next_step();
        Fs {
            tree,
            now,
            upper: Upper::default(),
            root: Tree::ROOT,
        }
    }

    /// `chroot(2)`: paths resolve from `dir` as from the root, `..` never leaves it, and an
    /// absolute symlink starts again at it. What changes is still recorded by its path
    /// from the snapshot's root.
    pub fn chroot(&mut self, dir: &[u8]) -> Result<(), PathError> {
        let (id, _) = self.lookup("chroot", dir, true)?;
        if !self.is_dir(id) {
            return fail("chroot", dir, Errno::NotDir);
        }
        self.root = id;
        Ok(())
    }

    /// Back to the snapshot's own root.
    pub fn unchroot(&mut self) {
        self.root = Tree::ROOT;
    }

    /// Starts a step on this snapshot: nothing is changed yet.
    pub fn begin(&mut self) {
        self.upper = Upper::default();
        self.tree.next_step();
    }

    /// Records what `entry` names as changed, and the directories above it as copied up.
    /// The root has no entry, and nothing above it.
    fn mark(&mut self, entry: Option<EntryId>) {
        if let Some(e) = entry {
            self.tree.mark(e);
        }
    }

    /// The absolute path of directory `dir` from the snapshot's root, symlinks aside.
    fn dir_path(&self, dir: NodeId) -> Vec<u8> {
        let mut names: Vec<&[u8]> = Vec::new();
        let mut at = dir;
        while let Some((parent, name)) = self.tree.up(at).and_then(|e| self.tree.entry(e)) {
            names.push(name);
            at = parent;
        }
        let mut p = Vec::new();
        for n in names.iter().rev() {
            p.push(b'/');
            p.extend_from_slice(n);
        }
        if p.is_empty() {
            p.push(b'/');
        }
        p
    }

    /// The absolute path of `name` in `dir`.
    fn path_in(&self, dir: NodeId, name: &[u8]) -> Vec<u8> {
        join(&self.dir_path(dir), name)
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.tree.node(id)
    }

    fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.tree.node_mut(id)
    }

    /// The snapshot's tree, to read.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// The tree, to change without recording the change as this step's: for what is no
    /// step's change. A merge's layers, applied onto the snapshot the next step starts
    /// from, and the export's last rewrite of the snapshot into its layers' form, which no
    /// layer follows.
    pub fn unrecorded_tree(&mut self) -> &mut Tree {
        &mut self.tree
    }

    pub fn is_dir(&self, id: NodeId) -> bool {
        self.tree.dir_len(id).is_some()
    }

    /// Walks `path` from the root as namei does; the last element's symlink is followed
    /// when `follow` is set, or when the path ends in a slash. Names are borrowed from the
    /// path and from the targets of the symlinks followed, never copied.
    fn walk(&self, path: &[u8], follow: bool) -> Result<Found, Errno> {
        if path.len() >= 4096 {
            return Err(Errno::NameTooLong);
        }
        // What remains to walk, last element first, and the directories walked through.
        let mut todo: Vec<&[u8]> = Vec::new();
        let mut trailing = path.ends_with(b"/");
        push_elements(&mut todo, path);
        let mut stack: Vec<NodeId> = vec![self.root];
        // The entry naming `current`, when it is no directory.
        let mut leaf: Option<EntryId> = None;
        let mut links = 0u32;
        let mut current = self.root;
        while let Some(name) = todo.pop() {
            let last = todo.is_empty();
            if !self.is_dir(current) {
                return Err(Errno::NotDir);
            }
            if name.len() > NAME_MAX {
                return Err(Errno::NameTooLong);
            }
            if name == b"." {
                if last {
                    trailing = true;
                }
                continue;
            }
            if name == b".." {
                if stack.len() > 1 {
                    stack.pop();
                }
                current = stack.last().copied().unwrap_or(self.root);
                if last {
                    trailing = true;
                }
                continue;
            }
            let Some((entry, child)) = self.tree.lookup(current, name) else {
                if last {
                    return Ok(Found::Missing {
                        dir: current,
                        name: name.to_vec(),
                    });
                }
                return Err(Errno::NoEnt);
            };
            if let Some(Node {
                kind: Kind::Symlink(target),
                ..
            }) = self.node(child)
                && (!last || follow || trailing)
            {
                links += 1;
                if links > MAX_LINKS {
                    return Err(Errno::Loop);
                }
                if target.is_empty() {
                    return Err(Errno::NoEnt);
                }
                if target.first() == Some(&b'/') {
                    stack.truncate(1);
                    current = self.root;
                }
                if target.ends_with(b"/") && last {
                    trailing = true;
                }
                push_elements(&mut todo, target);
                continue;
            }
            current = child;
            if self.is_dir(child) {
                stack.push(child);
            } else {
                leaf = Some(entry);
            }
        }
        if trailing && !self.is_dir(current) {
            return Err(Errno::NotDir);
        }
        let entry = if self.is_dir(current) {
            self.tree.up(current)
        } else {
            leaf
        };
        Ok(Found::Node(current, entry))
    }

    /// `lstat(2)`: what `path` names, its last symlink not followed.
    pub fn lstat(&self, path: &[u8]) -> Result<NodeId, PathError> {
        self.lookup("lstat", path, false).map(|(id, _)| id)
    }

    /// Where `path` leads, as [`Fs::lstat`] walks it, or why it leads nowhere.
    pub fn place(&self, path: &[u8]) -> Result<Place, Errno> {
        Ok(match self.walk(path, false)? {
            Found::Node(id, entry) => Place::Is(id, entry),
            Found::Missing { dir, name } => Place::Free(dir, name),
        })
    }

    /// `stat(2)`: what `path` names, its last symlink followed.
    pub fn stat(&self, path: &[u8]) -> Result<NodeId, PathError> {
        self.lookup("stat", path, true).map(|(id, _)| id)
    }

    /// What `path` names and the entry naming it.
    fn lookup(
        &self,
        op: &'static str,
        path: &[u8],
        follow: bool,
    ) -> Result<(NodeId, Option<EntryId>), PathError> {
        match self.walk(path, follow) {
            Ok(Found::Node(id, entry)) => Ok((id, entry)),
            Ok(Found::Missing { .. }) => fail(op, path, Errno::NoEnt),
            Err(e) => fail(op, path, e),
        }
    }

    /// Where a new entry `path` goes: its directory and name, or why it cannot go there.
    fn create_at(&self, op: &'static str, path: &[u8]) -> Result<(NodeId, Vec<u8>), PathError> {
        match self.walk(path, false) {
            Ok(Found::Missing { dir, name }) => Ok((dir, name)),
            Ok(Found::Node(..)) => fail(op, path, Errno::Exist),
            Err(e) => fail(op, path, e),
        }
    }

    /// Owner and inherited set-group-ID bit of an entry made in `dir`.
    fn new_owner(&self, dir: NodeId) -> (u32, u32, bool) {
        match self.node(dir) {
            Some(d) if u32::from(d.meta.mode) & S_ISGID != 0 => (0, d.meta.gid, true),
            _ => (0, 0, false),
        }
    }

    fn touch(&mut self, id: NodeId) {
        let now = self.now;
        if let Some(n) = self.node_mut(id) {
            n.meta.mtime = now.0;
            n.meta.mtime_nsec = now.1;
        }
    }

    fn add(
        &mut self,
        op: &'static str,
        path: &[u8],
        at: (NodeId, Vec<u8>),
        node: Node,
    ) -> Result<NodeId, PathError> {
        self.add_in(at.0, &at.1, node)
            .map(|(id, _)| id)
            .map_err(|errno| PathError {
                op,
                path: path.to_vec(),
                errno,
            })
    }

    /// Adds `node` as `name` in `dir`, which holds no such name: the node and its entry.
    fn add_in(&mut self, dir: NodeId, name: &[u8], node: Node) -> Result<(NodeId, Option<EntryId>), Errno> {
        let id = self.tree.insert(dir, name, node).map_err(|_| Errno::Inval)?;
        self.touch(dir);
        let entry = self.tree.lookup(dir, name).map(|(e, _)| e);
        self.mark(entry);
        Ok((id, entry))
    }

    fn fresh(&self, kind: Kind, mode: u32, dir: NodeId) -> Node {
        let (uid, gid, _) = self.new_owner(dir);
        Node {
            kind,
            meta: Meta {
                mode: (mode & 0o7777) as u16,
                uid,
                gid,
                mtime: self.now.0,
                mtime_nsec: self.now.1,
                xattrs: Default::default(),
            },
        }
    }

    /// `mkdir(2)` with `perm`'s permission and sticky bits, less the umask.
    pub fn mkdir(&mut self, path: &[u8], perm: u32) -> Result<NodeId, PathError> {
        let (dir, name) = self.create_at("mkdir", path)?;
        self.mkdir_in(dir, &name, perm)
            .map(|(id, _)| id)
            .map_err(|errno| PathError {
                op: "mkdir",
                path: path.to_vec(),
                errno,
            })
    }

    /// [`Fs::mkdir`] at a free place.
    pub fn mkdir_in(
        &mut self,
        dir: NodeId,
        name: &[u8],
        perm: u32,
    ) -> Result<(NodeId, Option<EntryId>), Errno> {
        let (_, _, sgid) = self.new_owner(dir);
        let mut mode = perm & 0o1777 & !UMASK;
        if sgid {
            mode |= S_ISGID;
        }
        if !self.upper.removed.is_empty() {
            let canon = self.path_in(dir, name);
            if self.upper.removed.contains(&canon) {
                self.upper.recreated.insert(canon);
            }
        }
        let node = self.fresh(Kind::Dir(Dir::default()), mode, dir);
        self.add_in(dir, name, node)
    }

    /// `mknod(2)`: a device, a FIFO, or (with `Kind::File`) an empty file.
    pub fn mknod(&mut self, path: &[u8], kind: Kind, perm: u32) -> Result<NodeId, PathError> {
        let at = self.create_at("mknod", path)?;
        let node = self.fresh(kind, perm & !UMASK, at.0);
        self.add("mknod", path, at, node)
    }

    /// [`Fs::mknod`] at a free place.
    pub fn mknod_in(
        &mut self,
        dir: NodeId,
        name: &[u8],
        kind: Kind,
        perm: u32,
    ) -> Result<(NodeId, Option<EntryId>), Errno> {
        let node = self.fresh(kind, perm & !UMASK, dir);
        self.add_in(dir, name, node)
    }

    /// `symlink(2)`.
    pub fn symlink(&mut self, target: &[u8], path: &[u8]) -> Result<NodeId, PathError> {
        let at = self.create_at("symlink", path).map_err(|e| PathError {
            path: linked(target, path),
            ..e
        })?;
        let node = self.fresh(Kind::Symlink(target.into()), 0o777, at.0);
        self.add("symlink", path, at, node)
    }

    /// [`Fs::symlink`] at a free place.
    pub fn symlink_in(
        &mut self,
        target: &[u8],
        dir: NodeId,
        name: &[u8],
    ) -> Result<(NodeId, Option<EntryId>), Errno> {
        let node = self.fresh(Kind::Symlink(target.into()), 0o777, dir);
        self.add_in(dir, name, node)
    }

    /// [`Fs::create`] at a free place: a new, empty file.
    pub fn create_in(
        &mut self,
        dir: NodeId,
        name: &[u8],
        perm: u32,
    ) -> Result<(NodeId, Option<EntryId>), Errno> {
        let node = self.fresh(Kind::File { size: 0, data: EMPTY }, perm & !UMASK, dir);
        self.add_in(dir, name, node)
    }

    /// `link(2)`, which does not follow `old` if it is a symlink.
    pub fn link(&mut self, old: &[u8], new: &[u8]) -> Result<(), PathError> {
        let both = linked(old, new);
        let target = self.lstat(old).map_err(|e| PathError {
            op: "link",
            path: both.clone(),
            errno: e.errno,
        })?;
        if self.is_dir(target) {
            return fail("link", &both, Errno::Perm);
        }
        let (dir, name) = self.create_at("link", new).map_err(|e| PathError {
            path: both.clone(),
            ..e
        })?;
        self.tree.link(dir, &name, target).map_err(|_| PathError {
            op: "link",
            path: both.clone(),
            errno: Errno::Inval,
        })?;
        self.touch(dir);
        let entry = self.tree.lookup(dir, &name).map(|(e, _)| e);
        self.mark(entry);
        Ok(())
    }

    /// `open(path, O_WRONLY|O_CREAT|O_TRUNC, perm)`, as `os.Create` and `os.OpenFile`
    /// call it: an existing file, its last symlink followed, is emptied.
    pub fn create(&mut self, path: &[u8], perm: u32) -> Result<NodeId, PathError> {
        match self.walk(path, true) {
            Ok(Found::Node(id, entry)) => {
                if self.is_dir(id) {
                    return fail("open", path, Errno::IsDir);
                }
                let now = self.now;
                if let Some(n) = self.node_mut(id)
                    && let Kind::File { size, .. } = &mut n.kind
                {
                    *size = 0;
                    n.meta.mtime = now.0;
                    n.meta.mtime_nsec = now.1;
                }
                self.mark(entry);
                Ok(id)
            }
            Ok(Found::Missing { dir, name }) => {
                let node = self.fresh(Kind::File { size: 0, data: EMPTY }, perm & !UMASK, dir);
                self.add("open", path, (dir, name), node)
            }
            Err(e) => fail("open", path, e),
        }
    }

    /// Writes the content of a file this step made or emptied, as copying into it does:
    /// its mtime is stamped.
    pub fn set_data(&mut self, id: NodeId, size: u64, data: DataRef) {
        let now = self.now;
        if let Some(n) = self.node_mut(id)
            && let Kind::File { size: s, data: d } = &mut n.kind
        {
            *s = size;
            *d = data;
            n.meta.mtime = now.0;
            n.meta.mtime_nsec = now.1;
        }
    }

    /// The entry `path` names, its last symlink not followed: its directory, name, node,
    /// and its id.
    fn entry(&self, op: &'static str, path: &[u8]) -> Result<(NodeId, Vec<u8>, NodeId, EntryId), PathError> {
        let (dir_path, name) = split(path);
        let name = name.to_vec();
        if name.is_empty() || name == b"." || name == b".." {
            return fail(
                op,
                path,
                if name.is_empty() {
                    Errno::Busy
                } else {
                    Errno::Inval
                },
            );
        }
        let dir = match self.walk(dir_path, true) {
            Ok(Found::Node(d, _)) => d,
            Ok(Found::Missing { .. }) => return fail(op, path, Errno::NoEnt),
            Err(e) => return fail(op, path, e),
        };
        if !self.is_dir(dir) {
            return fail(op, path, Errno::NotDir);
        }
        match self.tree.lookup(dir, &name) {
            Some((entry, id)) => Ok((dir, name, id, entry)),
            None => fail(op, path, Errno::NoEnt),
        }
    }

    /// Takes an entry out, recording its removal.
    fn take(&mut self, dir: NodeId, name: &[u8], entry: EntryId) {
        let canon = self.path_in(dir, name);
        self.tree.remove(dir, name);
        self.touch(dir);
        // The removed entry stays in its directory's list, stamped: a whiteout's place.
        self.mark(Some(entry));
        self.upper.removed.insert(canon);
    }

    /// `unlink(2)`.
    pub fn unlink(&mut self, path: &[u8]) -> Result<(), PathError> {
        let (dir, name, id, entry) = self.entry("unlink", path)?;
        if self.is_dir(id) {
            return fail("unlink", path, Errno::IsDir);
        }
        self.take(dir, &name, entry);
        Ok(())
    }

    /// `rmdir(2)`.
    pub fn rmdir(&mut self, path: &[u8]) -> Result<(), PathError> {
        let (dir, name, id, entry) = self.entry("rmdir", path)?;
        match self.tree.dir_len(id) {
            None => return fail("rmdir", path, Errno::NotDir),
            Some(n) if n > 0 => return fail("rmdir", path, Errno::NotEmpty),
            Some(_) => {}
        }
        self.take(dir, &name, entry);
        Ok(())
    }

    /// Go's `os.Remove`: unlink, then rmdir, and the error that means something.
    pub fn remove(&mut self, path: &[u8]) -> Result<(), PathError> {
        let e = match self.unlink(path) {
            Ok(()) => return Ok(()),
            Err(e) => e.errno,
        };
        let e1 = match self.rmdir(path) {
            Ok(()) => return Ok(()),
            Err(e) => e.errno,
        };
        fail("remove", path, if e1 != Errno::NotDir { e1 } else { e })
    }

    /// Go's `os.RemoveAll`: `path` and all below it, its last symlink not followed;
    /// nothing to remove is no error.
    pub fn remove_all(&mut self, path: &[u8]) -> Result<(), PathError> {
        if path.is_empty() {
            return Ok(());
        }
        if path == b"." || path.ends_with(b"/.") {
            return fail("RemoveAll", path, Errno::Inval);
        }
        match self.entry("RemoveAll", path) {
            Ok((dir, name, _, entry)) => {
                self.take(dir, &name, entry);
                Ok(())
            }
            Err(e) if matches!(e.errno, Errno::NoEnt | Errno::NotDir) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Go's `os.Rename` of `old`, which is not a directory, over `new`.
    pub fn rename(&mut self, old: &[u8], new: &[u8]) -> Result<(), PathError> {
        let both = linked(old, new);
        let lerr = |e: PathError| PathError {
            op: "rename",
            path: both.clone(),
            errno: e.errno,
        };
        // Go's os.Rename refuses an existing directory as the new name itself.
        if let Ok(existing) = self.lstat(new)
            && self.is_dir(existing)
        {
            return match self.lstat(old) {
                Err(e) => fail("rename", &both, e.errno),
                Ok(o) if o == existing => Ok(()),
                Ok(_) => fail("rename", &both, Errno::Exist),
            };
        }
        let (odir, oname, id, oentry) = self.entry("rename", old).map_err(lerr)?;
        if self.is_dir(id) {
            // overlayfs refuses to move a directory without redirect_dir, which BuildKit's
            // differ refuses too; nothing here moves one.
            return fail("rename", &both, Errno::XDev);
        }
        let (ndir, nname) = match self.walk(new, false) {
            Ok(Found::Missing { dir, name }) => (dir, name),
            Ok(Found::Node(existing, _)) => {
                let (dir, name, _, _) = self.entry("rename", new).map_err(lerr)?;
                if self.is_dir(existing) {
                    return fail("rename", &both, Errno::IsDir);
                }
                (dir, name)
            }
            Err(e) => return fail("rename", &both, e),
        };
        let ocanon = self.path_in(odir, &oname);
        self.tree
            .rename(odir, &oname, ndir, &nname)
            .map_err(|_| PathError {
                op: "rename",
                path: both.clone(),
                errno: Errno::Inval,
            })?;
        self.touch(odir);
        self.touch(ndir);
        self.mark(Some(oentry));
        self.upper.removed.insert(ocanon);
        let nentry = self.tree.lookup(ndir, &nname).map(|(e, _)| e);
        self.mark(nentry);
        Ok(())
    }

    /// `chmod(2)`, which follows symlinks: Go's `os.Chmod` with the set-ID and sticky bits
    /// it carries over.
    pub fn chmod(&mut self, path: &[u8], mode: u32) -> Result<(), PathError> {
        let (id, entry) = self.lookup("chmod", path, true)?;
        self.chmod_node(id, entry, mode);
        Ok(())
    }

    /// [`Fs::chmod`] of what a path led to, symlinks followed.
    pub fn chmod_node(&mut self, id: NodeId, entry: Option<EntryId>, mode: u32) {
        if let Some(n) = self.node_mut(id) {
            n.meta.mode = (mode & 0o7777) as u16;
        }
        self.mark(entry);
    }

    /// `lchown(2)`; an ID of `u32::MAX`, (uid_t)-1, is left as it is.
    pub fn lchown(&mut self, path: &[u8], uid: u32, gid: u32) -> Result<(), PathError> {
        let (id, entry) = self.lookup("lchown", path, false)?;
        self.lchown_node(id, entry, uid, gid);
        Ok(())
    }

    /// [`Fs::lchown`] of what a path led to.
    pub fn lchown_node(&mut self, id: NodeId, entry: Option<EntryId>, uid: u32, gid: u32) {
        self.mark(entry);
        let dir = self.is_dir(id);
        if let Some(n) = self.node_mut(id) {
            // (uid_t)-1 leaves an ID as it is.
            if uid != u32::MAX {
                n.meta.uid = uid;
            }
            if gid != u32::MAX {
                n.meta.gid = gid;
            }
            if !dir {
                let mut mode = u32::from(n.meta.mode);
                mode &= !S_ISUID;
                if mode & S_IXGRP != 0 {
                    mode &= !S_ISGID;
                }
                n.meta.mode = mode as u16;
                n.meta.xattrs.remove(CAPABILITY);
            }
        }
    }

    /// `utimensat(path, AT_SYMLINK_NOFOLLOW)`: the modification time.
    pub fn utimes(&mut self, path: &[u8], t: (i64, u32)) -> Result<(), PathError> {
        let (id, entry) = self.lookup("utimes", path, false)?;
        self.utimes_node(id, entry, t);
        Ok(())
    }

    /// [`Fs::utimes`] of what a path led to.
    pub fn utimes_node(&mut self, id: NodeId, entry: Option<EntryId>, t: (i64, u32)) {
        self.mark(entry);
        if let Some(n) = self.node_mut(id) {
            n.meta.mtime = t.0;
            n.meta.mtime_nsec = t.1;
        }
    }

    /// `lsetxattr(2)`, or `setxattr(2)` when `follow` is set.
    pub fn setxattr(&mut self, path: &[u8], key: &[u8], value: &[u8], follow: bool) -> Result<(), PathError> {
        let (id, entry) = self.lookup("setxattr", path, follow)?;
        self.setxattr_node(id, entry, key, value)
            .map_err(|errno| PathError {
                op: "setxattr",
                path: path.to_vec(),
                errno,
            })
    }

    /// [`Fs::setxattr`] of what a path led to.
    pub fn setxattr_node(
        &mut self,
        id: NodeId,
        entry: Option<EntryId>,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Errno> {
        let Some(n) = self.node(id) else {
            return Err(Errno::NoEnt);
        };
        let plain = matches!(n.kind, Kind::Dir(_) | Kind::File { .. });
        let errno = if key.starts_with(b"user.") {
            (!plain).then_some(Errno::Perm)
        } else if key.starts_with(b"trusted.overlay.") {
            Some(Errno::Perm)
        } else if key.starts_with(b"trusted.")
            || key.starts_with(b"security.")
            || key == b"system.posix_acl_access"
            || key == b"system.posix_acl_default"
        {
            None
        } else {
            Some(Errno::NotSup)
        };
        if let Some(errno) = errno {
            return Err(errno);
        }
        self.mark(entry);
        if let Some(n) = self.node_mut(id) {
            n.meta.xattrs.insert(key.to_vec(), value.to_vec());
        }
        Ok(())
    }

    /// `readlink(2)`.
    pub fn readlink(&self, path: &[u8]) -> Result<Vec<u8>, PathError> {
        let id = self.lstat(path).map_err(|e| PathError { op: "readlink", ..e })?;
        match self.node(id).map(|n| &n.kind) {
            Some(Kind::Symlink(t)) => Ok(t.to_vec()),
            _ => fail("readlink", path, Errno::Inval),
        }
    }

    /// Go's `os.ReadDir`: a directory's names, sorted.
    pub fn read_dir(&self, path: &[u8]) -> Result<Vec<Vec<u8>>, PathError> {
        let id = self.stat(path).map_err(|e| PathError { op: "open", ..e })?;
        if !self.is_dir(id) {
            return fail("readdirent", path, Errno::NotDir);
        }
        Ok(self
            .tree
            .entries(id)
            .into_iter()
            .map(|(n, _)| n.to_vec())
            .collect())
    }

    /// How many directory entries name each node: its link count, for what is not a
    /// directory.
    pub fn links(&self) -> Vec<u32> {
        let mut count = vec![0u32; self.tree.len()];
        let mut todo = vec![Tree::ROOT];
        let mut seen = vec![false; self.tree.len()];
        let mut entries = Vec::new();
        while let Some(dir) = todo.pop() {
            if self.tree.entries_into(dir, &mut entries) {
                for &(_, id) in &entries {
                    if let Some(c) = count.get_mut(id) {
                        *c += 1;
                    }
                    if self.is_dir(id) && seen.get(id) == Some(&false) {
                        if let Some(s) = seen.get_mut(id) {
                            *s = true;
                        }
                        todo.push(id);
                    }
                }
            }
        }
        count
    }
}

/// Go's `*os.LinkError` names two paths: `op old new: err`.
fn linked(old: &[u8], new: &[u8]) -> Vec<u8> {
    [old, b" ", new].concat()
}

/// The data of an empty file.
pub const EMPTY: DataRef = DataRef {
    source: u32::MAX,
    offset: 0,
};

/// Pushes `path`'s elements onto `todo` so the first is popped first.
fn push_elements<'p>(todo: &mut Vec<&'p [u8]>, path: &'p [u8]) {
    let at = todo.len();
    todo.extend(path.split(|&c| c == b'/').filter(|e| !e.is_empty()));
    if let Some(added) = todo.get_mut(at..) {
        added.reverse();
    }
}

/// A path's directory and last element, trailing slashes aside.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    let mut end = path.len();
    while end > 1 && path.get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    let trimmed = path.get(..end).unwrap_or(path);
    match trimmed.iter().rposition(|&c| c == b'/') {
        Some(i) => (
            trimmed.get(..i.max(1)).unwrap_or(b"/"),
            trimmed.get(i + 1..).unwrap_or_default(),
        ),
        None => (b"/", trimmed),
    }
}

/// Go's `filepath.Join`, for the absolute paths of a snapshot.
pub fn join(a: &[u8], b: &[u8]) -> Vec<u8> {
    go::join(&[a, b])
}
