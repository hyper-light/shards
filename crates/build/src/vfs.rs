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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use shards_dockerfile::go;
use shards_image::erofs::{DataRef, Kind, Meta, Node, NodeId, Tree};

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

/// What a path names, once the directories before its last element are found, with the
/// path that names it without symlinks: where overlayfs would copy it up to.
enum Found {
    Node(NodeId, Vec<u8>),
    /// The last element is not there; its directory is.
    Missing {
        dir: NodeId,
        name: Vec<u8>,
        path: Vec<u8>,
    },
}

/// What one step changed, as overlayfs's upper directory records it over the snapshot
/// the step started from: every path made, changed or removed, with the directories above
/// it; and directories made where one had been removed, which overlayfs makes opaque when
/// the snapshot below has the path.
#[derive(Debug, Clone, Default)]
pub struct Upper {
    pub touched: BTreeSet<Vec<u8>>,
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
    pub tree: Tree,
    /// Seconds and nanoseconds since 1970.
    pub now: (i64, u32),
    pub upper: Upper,
    /// Where paths resolve from: the snapshot's root, or a directory [`Fs::chroot`] made
    /// the root, and that directory's names from the snapshot's root.
    root: NodeId,
    root_names: Vec<Vec<u8>>,
}

impl Fs {
    pub fn new(tree: Tree, now: (i64, u32)) -> Fs {
        Fs {
            tree,
            now,
            upper: Upper::default(),
            root: Tree::ROOT,
            root_names: Vec::new(),
        }
    }

    /// `chroot(2)`: paths resolve from `dir` as from the root, `..` never leaves it, and an
    /// absolute symlink starts again at it. What changes is still recorded by its path
    /// from the snapshot's root.
    pub fn chroot(&mut self, dir: &[u8]) -> Result<(), PathError> {
        let (id, canon) = self.lookup("chroot", dir, true)?;
        if !self.is_dir(id) {
            return fail("chroot", dir, Errno::NotDir);
        }
        self.root = id;
        self.root_names = canon
            .split(|&c| c == b'/')
            .filter(|n| !n.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        Ok(())
    }

    /// Back to the snapshot's own root.
    pub fn unchroot(&mut self) {
        self.root = Tree::ROOT;
        self.root_names.clear();
    }

    /// Starts a step on this snapshot: nothing is changed yet.
    pub fn begin(&mut self) {
        self.upper = Upper::default();
    }

    /// Records `path` as changed, and the directories above it as copied up.
    fn mark(&mut self, path: &[u8]) {
        let mut p = path.to_vec();
        while p.len() > 1 {
            if !self.upper.touched.insert(p.clone()) {
                break;
            }
            let at = p.iter().rposition(|&c| c == b'/').unwrap_or(0);
            p.truncate(at.max(1));
        }
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.tree.node(id)
    }

    fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.tree.node_mut(id)
    }

    fn children(&self, id: NodeId) -> Option<&BTreeMap<Vec<u8>, NodeId>> {
        match &self.node(id)?.kind {
            Kind::Dir(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn is_dir(&self, id: NodeId) -> bool {
        self.children(id).is_some()
    }

    /// Walks `path` from the root as namei does; the last element's symlink is followed
    /// when `follow` is set, or when the path ends in a slash.
    fn walk(&self, path: &[u8], follow: bool) -> Result<Found, Errno> {
        if path.len() >= 4096 {
            return Err(Errno::NameTooLong);
        }
        // What remains to walk, last element first, and the directories walked through.
        let mut todo: Vec<Vec<u8>> = Vec::new();
        let mut trailing = path.ends_with(b"/");
        push_elements(&mut todo, path);
        let mut stack: Vec<NodeId> = vec![self.root];
        // The names of the directories on `stack` past the root, and of `current` when it
        // is not a directory.
        let mut names: Vec<Vec<u8>> = self.root_names.clone();
        let mut leaf: Option<Vec<u8>> = None;
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
                    names.pop();
                }
                current = stack.last().copied().unwrap_or(self.root);
                if last {
                    trailing = true;
                }
                continue;
            }
            let Some(child) = self.children(current).and_then(|c| c.get(&name)).copied() else {
                if last {
                    let path = canonical(&names, Some(&name));
                    return Ok(Found::Missing {
                        dir: current,
                        name,
                        path,
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
                    names.truncate(self.root_names.len());
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
                names.push(name);
            } else {
                leaf = Some(name);
            }
        }
        if trailing && !self.is_dir(current) {
            return Err(Errno::NotDir);
        }
        let path = canonical(
            &names,
            if self.is_dir(current) {
                None
            } else {
                leaf.as_deref()
            },
        );
        Ok(Found::Node(current, path))
    }

    /// `lstat(2)`: what `path` names, its last symlink not followed.
    pub fn lstat(&self, path: &[u8]) -> Result<NodeId, PathError> {
        self.lookup("lstat", path, false).map(|(id, _)| id)
    }

    /// `stat(2)`: what `path` names, its last symlink followed.
    pub fn stat(&self, path: &[u8]) -> Result<NodeId, PathError> {
        self.lookup("stat", path, true).map(|(id, _)| id)
    }

    /// What `path` names and its path without symlinks.
    fn lookup(&self, op: &'static str, path: &[u8], follow: bool) -> Result<(NodeId, Vec<u8>), PathError> {
        match self.walk(path, follow) {
            Ok(Found::Node(id, canon)) => Ok((id, canon)),
            Ok(Found::Missing { .. }) => fail(op, path, Errno::NoEnt),
            Err(e) => fail(op, path, e),
        }
    }

    /// Where a new entry `path` goes: its directory, name and path without symlinks, or
    /// why it cannot go there.
    fn create_at(&self, op: &'static str, path: &[u8]) -> Result<(NodeId, Vec<u8>, Vec<u8>), PathError> {
        match self.walk(path, false) {
            Ok(Found::Missing { dir, name, path }) => Ok((dir, name, path)),
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
        at: (NodeId, Vec<u8>, Vec<u8>),
        node: Node,
    ) -> Result<NodeId, PathError> {
        let (dir, name, canon) = at;
        let id = self.tree.insert(dir, &name, node).map_err(|_| PathError {
            op,
            path: path.to_vec(),
            errno: Errno::Inval,
        })?;
        self.touch(dir);
        self.mark(&canon);
        Ok(id)
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
                xattrs: BTreeMap::new(),
            },
        }
    }

    /// `mkdir(2)` with `perm`'s permission and sticky bits, less the umask.
    pub fn mkdir(&mut self, path: &[u8], perm: u32) -> Result<NodeId, PathError> {
        let at = self.create_at("mkdir", path)?;
        let (_, _, sgid) = self.new_owner(at.0);
        let mut mode = perm & 0o1777 & !UMASK;
        if sgid {
            mode |= S_ISGID;
        }
        if self.upper.removed.contains(&at.2) {
            self.upper.recreated.insert(at.2.clone());
        }
        let node = self.fresh(Kind::Dir(BTreeMap::new()), mode, at.0);
        self.add("mkdir", path, at, node)
    }

    /// `mknod(2)`: a device, a FIFO, or (with `Kind::File`) an empty file.
    pub fn mknod(&mut self, path: &[u8], kind: Kind, perm: u32) -> Result<NodeId, PathError> {
        let at = self.create_at("mknod", path)?;
        let node = self.fresh(kind, perm & !UMASK, at.0);
        self.add("mknod", path, at, node)
    }

    /// `symlink(2)`.
    pub fn symlink(&mut self, target: &[u8], path: &[u8]) -> Result<NodeId, PathError> {
        let at = self.create_at("symlink", path).map_err(|e| PathError {
            path: linked(target, path),
            ..e
        })?;
        let node = self.fresh(Kind::Symlink(target.to_vec()), 0o777, at.0);
        self.add("symlink", path, at, node)
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
        let (dir, name, canon) = self.create_at("link", new).map_err(|e| PathError {
            path: both.clone(),
            ..e
        })?;
        self.tree.link(dir, &name, target).map_err(|_| PathError {
            op: "link",
            path: both.clone(),
            errno: Errno::Inval,
        })?;
        self.touch(dir);
        self.mark(&canon);
        Ok(())
    }

    /// `open(path, O_WRONLY|O_CREAT|O_TRUNC, perm)`, as `os.Create` and `os.OpenFile`
    /// call it: an existing file, its last symlink followed, is emptied.
    pub fn create(&mut self, path: &[u8], perm: u32) -> Result<NodeId, PathError> {
        match self.walk(path, true) {
            Ok(Found::Node(id, canon)) => {
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
                self.mark(&canon);
                Ok(id)
            }
            Ok(Found::Missing {
                dir,
                name,
                path: canon,
            }) => {
                let node = self.fresh(Kind::File { size: 0, data: EMPTY }, perm & !UMASK, dir);
                self.add("open", path, (dir, name, canon), node)
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
    /// and its path without symlinks.
    fn entry(&self, op: &'static str, path: &[u8]) -> Result<(NodeId, Vec<u8>, NodeId, Vec<u8>), PathError> {
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
        let (dir, dir_canon) = match self.walk(dir_path, true) {
            Ok(Found::Node(d, c)) => (d, c),
            Ok(Found::Missing { .. }) => return fail(op, path, Errno::NoEnt),
            Err(e) => return fail(op, path, e),
        };
        let Some(entries) = self.children(dir) else {
            return fail(op, path, Errno::NotDir);
        };
        match entries.get(&name) {
            Some(&id) => Ok((dir, name.clone(), id, join(&dir_canon, &name))),
            None => fail(op, path, Errno::NoEnt),
        }
    }

    /// Takes an entry out, recording its removal.
    fn take(&mut self, dir: NodeId, name: &[u8], canon: &[u8]) {
        self.tree.remove(dir, name);
        self.touch(dir);
        self.mark(canon);
        self.upper.removed.insert(canon.to_vec());
    }

    /// `unlink(2)`.
    pub fn unlink(&mut self, path: &[u8]) -> Result<(), PathError> {
        let (dir, name, id, canon) = self.entry("unlink", path)?;
        if self.is_dir(id) {
            return fail("unlink", path, Errno::IsDir);
        }
        self.take(dir, &name, &canon);
        Ok(())
    }

    /// `rmdir(2)`.
    pub fn rmdir(&mut self, path: &[u8]) -> Result<(), PathError> {
        let (dir, name, id, canon) = self.entry("rmdir", path)?;
        match self.children(id) {
            None => return fail("rmdir", path, Errno::NotDir),
            Some(c) if !c.is_empty() => return fail("rmdir", path, Errno::NotEmpty),
            Some(_) => {}
        }
        self.take(dir, &name, &canon);
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
            Ok((dir, name, _, canon)) => {
                self.take(dir, &name, &canon);
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
        let (odir, oname, id, ocanon) = self.entry("rename", old).map_err(lerr)?;
        if self.is_dir(id) {
            // overlayfs refuses to move a directory without redirect_dir, which BuildKit's
            // differ refuses too; nothing here moves one.
            return fail("rename", &both, Errno::XDev);
        }
        let (ndir, nname, ncanon) = match self.walk(new, false) {
            Ok(Found::Missing { dir, name, path }) => (dir, name, path),
            Ok(Found::Node(existing, _)) => {
                let (dir, name, _, canon) = self.entry("rename", new).map_err(lerr)?;
                if self.is_dir(existing) {
                    return fail("rename", &both, Errno::IsDir);
                }
                (dir, name, canon)
            }
            Err(e) => return fail("rename", &both, e),
        };
        self.tree
            .rename(odir, &oname, ndir, &nname)
            .map_err(|_| PathError {
                op: "rename",
                path: both.clone(),
                errno: Errno::Inval,
            })?;
        self.touch(odir);
        self.touch(ndir);
        self.mark(&ocanon);
        self.upper.removed.insert(ocanon);
        self.mark(&ncanon);
        Ok(())
    }

    /// `chmod(2)`, which follows symlinks: Go's `os.Chmod` with the set-ID and sticky bits
    /// it carries over.
    pub fn chmod(&mut self, path: &[u8], mode: u32) -> Result<(), PathError> {
        let (id, canon) = self.lookup("chmod", path, true)?;
        if let Some(n) = self.node_mut(id) {
            n.meta.mode = (mode & 0o7777) as u16;
        }
        self.mark(&canon);
        Ok(())
    }

    /// `lchown(2)`; an ID of `u32::MAX`, (uid_t)-1, is left as it is.
    pub fn lchown(&mut self, path: &[u8], uid: u32, gid: u32) -> Result<(), PathError> {
        let (id, canon) = self.lookup("lchown", path, false)?;
        self.mark(&canon);
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
        Ok(())
    }

    /// `utimensat(path, AT_SYMLINK_NOFOLLOW)`: the modification time.
    pub fn utimes(&mut self, path: &[u8], t: (i64, u32)) -> Result<(), PathError> {
        let (id, canon) = self.lookup("utimes", path, false)?;
        self.mark(&canon);
        if let Some(n) = self.node_mut(id) {
            n.meta.mtime = t.0;
            n.meta.mtime_nsec = t.1;
        }
        Ok(())
    }

    /// `lsetxattr(2)`, or `setxattr(2)` when `follow` is set.
    pub fn setxattr(&mut self, path: &[u8], key: &[u8], value: &[u8], follow: bool) -> Result<(), PathError> {
        let (id, canon) = self.lookup("setxattr", path, follow)?;
        let Some(n) = self.node(id) else {
            return fail("setxattr", path, Errno::NoEnt);
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
            return fail("setxattr", path, errno);
        }
        self.mark(&canon);
        if let Some(n) = self.node_mut(id) {
            n.meta.xattrs.insert(key.to_vec(), value.to_vec());
        }
        Ok(())
    }

    /// `readlink(2)`.
    pub fn readlink(&self, path: &[u8]) -> Result<Vec<u8>, PathError> {
        let id = self.lstat(path).map_err(|e| PathError { op: "readlink", ..e })?;
        match self.node(id).map(|n| &n.kind) {
            Some(Kind::Symlink(t)) => Ok(t.clone()),
            _ => fail("readlink", path, Errno::Inval),
        }
    }

    /// Go's `os.ReadDir`: a directory's names, sorted.
    pub fn read_dir(&self, path: &[u8]) -> Result<Vec<Vec<u8>>, PathError> {
        let id = self.stat(path).map_err(|e| PathError { op: "open", ..e })?;
        match self.children(id) {
            Some(c) => Ok(c.keys().cloned().collect()),
            None => fail("readdirent", path, Errno::NotDir),
        }
    }

    /// How many directory entries name each node: its link count, for what is not a
    /// directory.
    pub fn links(&self) -> Vec<u32> {
        let mut count = vec![0u32; self.tree.len()];
        let mut todo = vec![Tree::ROOT];
        let mut seen = vec![false; self.tree.len()];
        while let Some(dir) = todo.pop() {
            if let Some(entries) = self.children(dir) {
                for &id in entries.values() {
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

/// The absolute path of `names`, then `last`.
fn canonical(names: &[Vec<u8>], last: Option<&[u8]>) -> Vec<u8> {
    let mut p = Vec::new();
    for n in names.iter().map(Vec::as_slice).chain(last) {
        p.push(b'/');
        p.extend_from_slice(n);
    }
    if p.is_empty() {
        p.push(b'/');
    }
    p
}

/// Pushes `path`'s elements onto `todo` so the first is popped first.
fn push_elements(todo: &mut Vec<Vec<u8>>, path: &[u8]) {
    let elements: Vec<&[u8]> = path.split(|&c| c == b'/').filter(|e| !e.is_empty()).collect();
    for e in elements.into_iter().rev() {
        todo.push(e.to_vec());
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
