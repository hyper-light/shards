//! A FUSE server over a host directory, for virtio-fs (Linux include/uapi/linux/fuse.h,
//! protocol 7.31 and later; Documentation/filesystems/fuse.rst).
//!
//! Every operation is relative to a descriptor the server holds: the shared directory's,
//! a directory node's own, or a node's parent's with its name, and none follows a symlink
//! it was not asked to read. So a guest reaches nothing outside the directory it was given,
//! whatever it renames or links meanwhile.
//!
//! Ownership is Docker Desktop's (measured: Docker Desktop 4.66.1's virtiofs, 2026-10-05):
//! the host cannot give a rootless process's files another owner, so a file's guest owner is
//! kept in its `com.docker.grpcfuse.ownership` extended attribute, as `{"UID":U,"GID":G,
//! "mode":M}` with the mode's octal digits, and a file without one is root's. A directory
//! Docker Desktop shared reads the same here. Unlike Docker Desktop's, a file a non-root
//! process makes is that process's, and the guest kernel checks permissions
//! (`default_permissions`) as on any Linux filesystem.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::os::fd::FromRawFd as _;
use std::sync::{Mutex, PoisonError};

use super::host::{self, AsRaw as _, Stat, linux};
pub use super::proto::{HEADER_BYTES, IN_HEADER_BYTES, MAX_PAGES, MAX_TABLES, MAX_WRITE};
use super::proto::{OUT_HEADER_BYTES, op};
use crate::snapshot::codec::{Reader, Writer};

const ROOT: u64 = 1;
/// How long the guest may trust what it was told of a name or its attributes: virtiofsd's
/// `cache=auto` second, as the host may change the directory meanwhile.
const VALID_S: u64 = 1;
// `mode_t` is u16 on macOS and u32 on Linux: its values as the guest's u32.
#[allow(clippy::unnecessary_cast)]
const S_IFMT: u32 = host::S_IFMT as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFDIR: u32 = host::S_IFDIR as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFREG: u32 = host::S_IFREG as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFLNK: u32 = host::S_IFLNK as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFCHR: u32 = host::S_IFCHR as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFBLK: u32 = host::S_IFBLK as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFIFO: u32 = host::S_IFIFO as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFSOCK: u32 = host::S_IFSOCK as u32;
#[allow(clippy::unnecessary_cast)]
const S_ISUID: u32 = host::S_ISUID as u32;
#[allow(clippy::unnecessary_cast)]
const S_ISGID: u32 = host::S_ISGID as u32;

/// A stat's mode as the guest's u32.
fn mode_of(st: &Stat) -> u32 {
    st.mode
}

const OWNER_XATTR: &str = "com.docker.grpcfuse.ownership";

/// FUSE_INIT's flags this server takes up.
mod init {
    pub const ASYNC_READ: u32 = 1 << 0;
    pub const ATOMIC_O_TRUNC: u32 = 1 << 3;
    pub const BIG_WRITES: u32 = 1 << 5;
    pub const DO_READDIRPLUS: u32 = 1 << 13;
    pub const READDIRPLUS_AUTO: u32 = 1 << 14;
    pub const PARALLEL_DIROPS: u32 = 1 << 18;
    pub const MAX_PAGES: u32 = 1 << 22;
    pub const CACHE_SYMLINKS: u32 = 1 << 23;
    /// A DAX window's offsets are whole host pages (D189): `map_alignment` says how large.
    pub const MAP_ALIGNMENT: u32 = 1 << 26;
    /// `flags2` follows.
    pub const INIT_EXT: u32 = 1 << 30;
    /// `flags2`'s: the server says which files the guest maps into its DAX window
    /// (FUSE_HAS_INODE_DAX, bit 33 of the flags), by `fuse_attr`'s FUSE_ATTR_DAX.
    pub const HAS_INODE_DAX: u32 = 1 << 1;
}

/// `fuse_attr.flags`' FUSE_ATTR_DAX: the guest maps this file into its DAX window (D189).
const ATTR_DAX: u32 = 1 << 1;
/// The smallest file the guest is told to map into its DAX window, 1 MiB: the smallest of
/// the sizes measured from 4 KiB to 2 MiB at which, and at each larger one, a file read,
/// re-read and overwritten through a window was no worse than through FUSE at p50 and p99
/// beyond the rounds' spread, held no more memory, and left a working tree's files no slower
/// to read, on the Mac and on Fedora's performance and efficient cores (PM M338): from
/// 256 KiB on the Mac and Fedora's efficient cores, from 1 MiB on its performance cores,
/// where a 256 KiB file's cold overwrite took 251 to 258 µs at p50 through a window and 202
/// to 209 through FUSE. Smaller files are read through FUSE, which a mapping, a round trip
/// to the share process and a host mmap(2), costs them more than it saves.
pub const DAX_SMALLEST: u64 = 1 << 20;

mod fattr {
    pub const MODE: u32 = 1 << 0;
    pub const UID: u32 = 1 << 1;
    pub const GID: u32 = 1 << 2;
    pub const SIZE: u32 = 1 << 3;
    pub const ATIME: u32 = 1 << 4;
    pub const MTIME: u32 = 1 << 5;
    pub const FH: u32 = 1 << 6;
    pub const ATIME_NOW: u32 = 1 << 7;
    pub const MTIME_NOW: u32 = 1 << 8;
    pub const KILL_SUIDGID: u32 = 1 << 11;
}

/// A node the guest knows: by its parent's node and its name there, kept current through
/// the guest's renames and lookups, and by what it is (`key`, its device and inode). A
/// directory's descriptor is in `State::dirs` while there is room for it, and opened again
/// along its path when it is needed and not there (audit V09). `lookups` is the kernel's
/// count of it (FORGET's).
#[derive(Debug)]
struct Node {
    dir: bool,
    parent: u64,
    name: CString,
    /// The other names the guest has looked a file up by (its hard links), each its
    /// directory and name: where the guest unlinks or renames away the one it was last
    /// looked up by, as overlayfs does with the links of its shared whiteout, another of
    /// them still reaches it ([`reach`]). None for a directory.
    links: Vec<(u64, CString)>,
    lookups: u64,
    key: (u64, u64),
    /// The node of the volume it is in, in a join share ([`Server::joined`]); none
    /// ([`NO_VOLUME`]) in a share of one directory, and the join share's root's own.
    volume: u64,
}

/// The prefix of a file [`ghost`] makes, which a daemon's start removes where a crash left
/// one before its removal.
pub const GHOST_PREFIX: &str = "ghost-";

/// A file holding `bytes`, made in directory `dir` and removed from it at once: the
/// descriptor of a file no path reaches, as its guest held one before its save (D176).
fn ghost(dir: host::Raw, bytes: &[u8]) -> Result<host::Owned, String> {
    let mut random = [0u8; 16];
    crate::platform::fill_random(&mut random).map_err(|e| format!("a removed file's name: {e}"))?;
    let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let name = CString::new(format!("{GHOST_PREFIX}{name}")).map_err(|_| "a removed file's name")?;
    let fd = open_at(
        dir,
        &name,
        host::O_RDWR | host::O_CREAT | host::O_EXCL | host::O_CLOEXEC,
        0o600,
    )
    .map_err(|e| format!("a removed file, made again: errno {e}"))?;
    // SAFETY: unlinkat(2) of the name just made in a directory we hold.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 {
        return Err(format!(
            "a removed file, made again: {}",
            std::io::Error::last_os_error()
        ));
    }
    let file = std::fs::File::from(fd);
    crate::platform::write_all_at(&file, bytes, 0).map_err(|e| format!("a removed file's bytes: {e}"))?;
    Ok(file.into())
}

/// No volume's: a node of a share of one directory, or a join share's root. Node IDs start
/// at [`ROOT`], so none is 0.
const NO_VOLUME: u64 = 0;

/// A join share's volume (D119): a directory a container joining the microVM's network is
/// given, reached from the share's empty root by its name, read-only or not, or of its one
/// name, held for as long as it is served.
#[derive(Debug)]
struct Volume {
    fd: host::Owned,
    read_only: bool,
    only: Option<CString>,
}

/// A file or directory the guest opened: its node, and a file's host flags, less those
/// that act once (O_CREAT, O_EXCL, O_TRUNC). Its descriptor is in `State::fds` while there
/// is room for it, and opened again along its node's path when it is needed and not there,
/// checked to be the node still. A node no descriptor could be opened for again (the guest
/// unlinked it or renamed another over it, or gave it a mode that refuses its handles) serves
/// them from a descriptor pinned for it (`State::pins`; audit V09).
#[derive(Debug)]
struct Handle {
    node: u64,
    flags: Option<host::c_int>,
}

/// A node's open handles, and how many may read and write: what a descriptor pinned for
/// them must allow.
#[derive(Debug, Default)]
struct Opened {
    handles: u64,
    reading: u64,
    writing: u64,
}

/// What a descriptor in `State::fds` is held for: a directory node, or a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Node(u64),
    Handle(u64),
}

/// A descriptor in `State::fds`: a plain one, or a directory handle's stream.
#[derive(Debug)]
enum Held {
    Fd(host::Owned),
    Dir(DirStream),
}

#[derive(Debug)]
struct State {
    /// The shared directory, held for as long as the server: a join share's an empty one,
    /// its volumes reached from it by name.
    root: host::Owned,
    nodes: HashMap<u64, Node>,
    /// Each node by its volume and what it is (its device and inode): one directory shared
    /// as two volumes, read-only and not, is two nodes, never one whose volume is either's.
    by_key: HashMap<(u64, u64, u64), u64>,
    /// A join share's (D119): its volumes, by their nodes, and their nodes by their names.
    joined: bool,
    volumes: HashMap<u64, Volume>,
    named: HashMap<CString, u64>,
    /// How many node IDs have been drawn, and the keys they are drawn with: each a keyed
    /// hash (SipHash) of its count, so that no ID can be guessed from another, nor a handle
    /// to a node forged (open_by_handle_at, which takes a node's ID): a join share's guest
    /// holds every joiner's nodes in one file system, and each joiner may see its own IDs.
    next_node: u64,
    ids: std::hash::RandomState,
    handles: HashMap<u64, Handle>,
    next_handle: u64,
    /// Each node's open handles.
    opened: HashMap<u64, Opened>,
    fds: Fds,
    /// Descriptors of nodes the guest has open that none could be opened for again, with the
    /// access mode they were opened for: since it unlinked them or renamed another over them,
    /// or made them or changed them to a mode that refuses their handles. Let go with their
    /// node's last handle, never before, and the only descriptors counted against the
    /// budget.
    pins: HashMap<u64, (host::Owned, host::c_int)>,
    /// Nodes whose last name the guest removed with no room to pin them, and the first handle
    /// opened after: those before it are stale once their descriptors are let go, as the
    /// number the node's path might lead to is free to be another file's.
    unreached: HashMap<u64, u64>,
    /// The descriptors the server may hold between requests ([`Limits::budget`]): its pins,
    /// and in the room they leave, its directories' and handles' descriptors.
    budget: u64,
    /// Where a restored server makes the files its guest held open that no path reaches,
    /// each removed as it is made, for its pins (D176): the share process's private
    /// directory, none for a server that keeps none.
    ghosts: Option<host::Owned>,
    /// The budgets over the share process's cgroups, which its host's copy of what the guest
    /// reads is dropped past (D188): none for a server that keeps every copy.
    headroom: Option<super::headroom::Headroom>,
}

/// The descriptors of the directories the guest named and of the files and directories it
/// opened, as many as there is room for, the least recently used let go first. They are let
/// go between requests, never during one, so that a descriptor a request took stays open
/// until it answers.
#[derive(Debug, Default)]
struct Fds {
    open: HashMap<Slot, (Held, u64)>,
    /// Each slot's last use, oldest first.
    by_use: BTreeMap<u64, Slot>,
    clock: u64,
}

impl Fds {
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// `slot`'s descriptor, if held, now the most recently used.
    fn get(&mut self, slot: Slot) -> Option<&mut Held> {
        let now = self.tick();
        let (held, used) = self.open.get_mut(&slot)?;
        self.by_use.remove(used);
        *used = now;
        self.by_use.insert(now, slot);
        Some(held)
    }

    fn insert(&mut self, slot: Slot, held: Held) {
        self.remove(slot);
        let now = self.tick();
        self.open.insert(slot, (held, now));
        self.by_use.insert(now, slot);
    }

    fn remove(&mut self, slot: Slot) {
        if let Some((_, used)) = self.open.remove(&slot) {
            self.by_use.remove(&used);
        }
    }

    /// Lets go of the least recently used until `room` are held.
    fn trim(&mut self, room: u64) {
        while u64::try_from(self.open.len()).unwrap_or(u64::MAX) > room {
            let Some((_, slot)) = self.by_use.pop_first() else {
                return;
            };
            self.open.remove(&slot);
        }
    }
}

/// The server of one shared directory.
#[derive(Debug)]
pub struct Server {
    read_only: bool,
    /// The one name of the directory the guest may reach, for a file bound alone: the
    /// directory is shared for it, and nothing else of the directory shows.
    only: Option<CString>,
    state: Mutex<State>,
}

/// A failed operation's Linux errno.
type Errno = i32;

fn last() -> Errno {
    host::linux_errno(
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    )
}

const EPERM: Errno = 1;
const EIO: Errno = 5;
const EEXIST: Errno = 17;
const EXDEV: Errno = 18;
const EMFILE: Errno = 24;
#[cfg(target_os = "linux")]
const ENAMETOOLONG: Errno = 36;
const ELOOP: Errno = 40;
const ESTALE: Errno = 116;
const ENOENT: Errno = 2;
const EACCES: Errno = 13;
const EBUSY: Errno = 16;
const EBADF: Errno = 9;
const EINVAL: Errno = 22;
const EROFS: Errno = 30;
const ENOSYS: Errno = 38;
const ENOTDIR: Errno = 20;
const EISDIR: Errno = 21;
const ERANGE: Errno = 34;
const ENODATA: Errno = 61;
const EOPNOTSUPP: Errno = 95;

/// Little-endian fields of a request, in order.
struct Args<'a>(&'a [u8]);

impl<'a> Args<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Errno> {
        let (head, rest) = self.0.split_at_checked(n).ok_or(EINVAL)?;
        self.0 = rest;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32, Errno> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| EINVAL)?))
    }
    fn u64(&mut self) -> Result<u64, Errno> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| EINVAL)?))
    }
    /// A NUL-terminated name, which may not hold a `/` or be `.` or `..`.
    fn name(&mut self) -> Result<CString, Errno> {
        let end = self.0.iter().position(|&b| b == 0).ok_or(EINVAL)?;
        let name = self.take(end)?;
        self.take(1)?;
        if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
            return Err(EINVAL);
        }
        CString::new(name).map_err(|_| EINVAL)
    }
    /// A NUL-terminated string, as given.
    fn cstr(&mut self) -> Result<CString, Errno> {
        let end = self.0.iter().position(|&b| b == 0).ok_or(EINVAL)?;
        let s = self.take(end)?;
        self.take(1)?;
        CString::new(s).map_err(|_| EINVAL)
    }
    fn rest(&self) -> &'a [u8] {
        self.0
    }
}

/// A guest's file owner, as kept on the host (Docker Desktop's attribute), and what else
/// of it the host cannot hold.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Owner {
    uid: u32,
    gid: u32,
    /// The permission bits it was given, where the attribute holds them: where they differ
    /// from the host file's, which always lets its owner, the share process, reach it.
    mode: Option<u32>,
    /// The kind of node it is, where the host made an empty regular file in its place (a
    /// device node, which the host makes only for a privileged process): `S_IFCHR`,
    /// `S_IFBLK`, `S_IFIFO` or `S_IFSOCK`, kept as `"kind"`.
    kind: Option<u32>,
    /// Its device number as the guest gave it (Linux's encoding), kept as `"rdev"`.
    rdev: Option<u32>,
}

/// What a node is given as it is made: its permission bits, and, where the host makes
/// another kind of file in its place, its kind and device number.
#[derive(Clone, Copy, Default)]
struct Kept {
    mode: Option<u32>,
    kind: Option<u32>,
    rdev: Option<u32>,
}

impl Kept {
    fn mode(mode: u32) -> Kept {
        Kept {
            mode: Some(mode & 0o7777),
            ..Kept::default()
        }
    }
}

impl Owner {
    /// `{"UID":U,"GID":G,"mode":M}`, M the mode's octal digits read as decimal; with
    /// `"kind":K` (octal, as M) and `"rdev":R` where the node is not the host's kind.
    fn parse(bytes: &[u8]) -> Option<Owner> {
        let text = std::str::from_utf8(bytes).ok()?;
        let field = |key: &str| -> Option<u64> {
            let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
            let rest = text.get(at..)?.trim_start().strip_prefix(':')?.trim_start();
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        };
        let octal = |key: &str| field(key).and_then(|m| u32::from_str_radix(&m.to_string(), 8).ok());
        Some(Owner {
            uid: u32::try_from(field("UID")?).ok()?,
            gid: u32::try_from(field("GID")?).ok()?,
            mode: octal("mode"),
            kind: octal("kind").filter(|k| matches!(*k, S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK)),
            rdev: field("rdev").and_then(|r| u32::try_from(r).ok()),
        })
    }

    fn encode(&self) -> String {
        let mut out = format!("{{\"UID\":{},\"GID\":{}", self.uid, self.gid);
        if let Some(m) = self.mode {
            out.push_str(&format!(",\"mode\":{:o}", m & 0o7777));
        }
        if let Some(k) = self.kind {
            out.push_str(&format!(",\"kind\":{k:o},\"rdev\":{}", self.rdev.unwrap_or(0)));
        }
        out.push('}');
        out
    }
}

/// The permission bits the host file is given for `perm`, which the guest asked for: the
/// guest's, with its owner's access added (`rwx` for a directory, `rw` for anything else),
/// so that the share process, which is the host file's owner and has no privilege over
/// it, may always reach it. The guest's own are kept where they differ ([`Owner::mode`]),
/// and the guest's kernel checks them (`default_permissions`). A directory overlayfs
/// makes its work directory in with mode 0 (fs/overlayfs/super.c, ovl_workdir_create)
/// is the guest root's to write in, and was not the host's.
fn host_perm(perm: u32, dir: bool) -> u32 {
    (perm & 0o7777) | if dir { 0o700 } else { 0o600 }
}

fn cstring(s: &str) -> Result<CString, Errno> {
    CString::new(s).map_err(|_| EINVAL)
}

/// `fd`'s attribute `name`, or none.
fn fget_xattr(fd: host::Raw, name: &CStr) -> Result<Option<Vec<u8>>, Errno> {
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: fgetxattr(2) into a buffer of its length.
        #[cfg(target_os = "macos")]
        let n = unsafe { libc::fgetxattr(fd, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0) };
        // SAFETY: as above.
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::fgetxattr(fd, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            buf.truncate(usize::try_from(n).unwrap_or(0));
            return Ok(Some(buf));
        }
        match last() {
            ENODATA => return Ok(None),
            ERANGE if buf.len() < 1 << 16 => buf.resize(buf.len() * 4, 0),
            e => return Err(e),
        }
    }
}

fn fset_xattr(fd: host::Raw, name: &CStr, value: &[u8], flags: u32) -> Result<(), Errno> {
    let mut host_flags = 0;
    if flags & linux::XATTR_CREATE != 0 {
        host_flags |= libc::XATTR_CREATE;
    }
    if flags & linux::XATTR_REPLACE != 0 {
        host_flags |= libc::XATTR_REPLACE;
    }
    // SAFETY: fsetxattr(2) of a buffer of its length.
    #[cfg(target_os = "macos")]
    let r = unsafe {
        libc::fsetxattr(
            fd,
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            host_flags,
        )
    };
    // SAFETY: as above.
    #[cfg(target_os = "linux")]
    let r = unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), host_flags) };
    if r != 0 {
        return Err(last());
    }
    Ok(())
}

/// The name the host keeps a guest's ownership under: Docker Desktop's on macOS, which
/// keeps no namespaces; in Linux's `user.` namespace on Linux.
fn owner_xattr() -> Result<CString, Errno> {
    #[cfg(target_os = "macos")]
    {
        cstring(OWNER_XATTR)
    }
    #[cfg(target_os = "linux")]
    {
        cstring(&format!("user.{OWNER_XATTR}"))
    }
}

fn stat_fd(fd: host::Raw) -> Result<Stat, Errno> {
    // SAFETY: an all-zero stat is a valid out-parameter for fstat(2).
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat(2) of a descriptor we hold.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(last());
    }
    Ok(Stat::from(&st))
}

fn stat_at(dir: host::Raw, name: &CStr) -> Result<Stat, Errno> {
    // SAFETY: as stat_fd.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstatat(2) of a NUL-terminated name in a directory we hold, not following it.
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, host::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(last());
    }
    Ok(Stat::from(&st))
}

/// What the symlink `name` in `dir` holds (readlinkat(2)).
fn readlink_at(dir: host::Raw, name: &CStr) -> Result<Vec<u8>, Errno> {
    let mut buf = vec![0u8; 4096];
    // SAFETY: readlinkat(2) into a buffer of its length.
    let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(last());
    }
    buf.truncate(usize::try_from(n).unwrap_or(0));
    Ok(buf)
}

/// A symlink `name` in `dir` holding `target` (symlinkat(2)).
fn symlink_at(target: &CStr, dir: host::Raw, name: &CStr) -> Result<(), Errno> {
    // SAFETY: symlinkat(2) of NUL-terminated strings in a directory we hold.
    if unsafe { libc::symlinkat(target.as_ptr(), dir, name.as_ptr()) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// A node `name` in `dir` of `mode` (its kind and permissions) and the guest's device
/// number `rdev` (mknodat(2)).
fn mknod_at(dir: host::Raw, name: &CStr, mode: u32, rdev: u32) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: mknodat(2) of a NUL-terminated name in a directory we hold.
    if unsafe { libc::mknodat(dir, name.as_ptr(), mode as host::mode_t, host::decode_dev(rdev)) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// A directory `name` in `dir` of the host's permissions `perm` (mkdirat(2)).
fn mkdir_at(dir: host::Raw, name: &CStr, perm: u32) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: mkdirat(2) of a NUL-terminated name in a directory we hold.
    if unsafe { libc::mkdirat(dir, name.as_ptr(), perm as host::mode_t) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// Removes `name` from `dir`, a directory if `rmdir` (unlinkat(2)).
fn unlink_at(dir: host::Raw, name: &CStr, rmdir: bool) -> Result<(), Errno> {
    let flags = if rmdir { host::AT_REMOVEDIR } else { 0 };
    // SAFETY: unlinkat(2) of a NUL-terminated name in a directory we hold.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// Another name, `name` in `dir`, of `oname` in `odir` (linkat(2)).
fn link_at(odir: host::Raw, oname: &CStr, dir: host::Raw, name: &CStr) -> Result<(), Errno> {
    // SAFETY: linkat(2) of NUL-terminated names in directories we hold.
    if unsafe { libc::linkat(odir, oname.as_ptr(), dir, name.as_ptr(), 0) } != 0 {
        return Err(last());
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The names this thread's server has opened, for a test of what it never opens.
    static OPENED: std::cell::RefCell<Vec<CString>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn open_at(
    dir: host::Raw,
    name: &CStr,
    flags: host::c_int,
    mode: host::c_uint,
) -> Result<host::Owned, Errno> {
    #[cfg(test)]
    OPENED.with(|o| o.borrow_mut().push(name.to_owned()));
    // SAFETY: openat(2) of a NUL-terminated name in a directory we hold.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | host::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(last());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { host::Owned::from_raw_fd(fd) })
}

/// `name` in `dir` opened for a guest's OPEN or CREATE, with `flags` (`host::open_flags`'),
/// if it is a regular file. A guest kernel opens FIFOs, sockets and device nodes itself and
/// sends no FUSE request to; one that asks would have this process wait on a FIFO's other
/// end for good, or open a host device, so they are refused (EBADF), as QEMU's virtiofsd
/// refuses them (`lo_inode_open`, CVE-2020-35517; audit V02). The name is looked at first,
/// so no special file is opened, then opened without waiting and looked at again, for one
/// put there meanwhile.
fn open_regular(
    dir: host::Raw,
    name: &CStr,
    flags: host::c_int,
    mode: host::c_uint,
) -> Result<(host::Owned, Stat), Errno> {
    let exclusive = flags & (host::O_CREAT | host::O_EXCL) == host::O_CREAT | host::O_EXCL;
    match stat_at(dir, name) {
        // An exclusive create of a name that exists fails at the open, opening nothing.
        Ok(st) if !exclusive && mode_of(&st) & S_IFMT != S_IFREG => return Err(EBADF),
        Ok(_) => {}
        Err(ENOENT) if flags & host::O_CREAT != 0 => {}
        Err(e) => return Err(e),
    }
    let fd = open_at(dir, name, flags | host::O_NONBLOCK | host::O_NOCTTY, mode)?;
    let st = stat_fd(fd.raw())?;
    if mode_of(&st) & S_IFMT != S_IFREG {
        return Err(EBADF);
    }
    // The status flags the guest asked for, without O_NONBLOCK: F_SETFL takes those of
    // `flags` and ignores its access mode and creation flags (POSIX fcntl(2)).
    // SAFETY: fcntl(2) of a descriptor we hold.
    if flags & host::O_NONBLOCK == 0 && unsafe { libc::fcntl(fd.raw(), libc::F_SETFL, flags) } != 0 {
        return Err(last());
    }
    Ok((fd, st))
}

/// Changes the mode of `name` in `dir` without following it. fchmodat(2) without
/// AT_SYMLINK_NOFOLLOW follows a final symlink: a guest's link to a host file outside the
/// share would have that file's mode changed (audit V01). A symlink's own mode is not
/// changed, as Linux changes none (EOPNOTSUPP: fs/attr.c `notify_change` since 6.6, and
/// glibc's and musl's fchmodat with AT_SYMLINK_NOFOLLOW before it).
fn chmod_at(dir: host::Raw, name: &CStr, mode: u32) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        // Linux's fchmodat(2) takes no AT_SYMLINK_NOFOLLOW before fchmodat2 (6.6). The
        // name is opened as a path, never followed, and the file changed through its
        // /proc/self/fd link, which reaches that file whatever is at the name by then
        // (musl's src/stat/fchmodat.c, virtiofsd's setattr).
        let fd = open_at(dir, name, libc::O_PATH | host::O_NOFOLLOW, 0)?;
        if mode_of(&stat_fd(fd.raw())?) & S_IFMT == S_IFLNK {
            return Err(EOPNOTSUPP);
        }
        let link = CString::new(format!("/proc/self/fd/{}", fd.raw())).map_err(|_| EINVAL)?;
        // SAFETY: fchmodat(2) of a NUL-terminated path.
        if unsafe { libc::fchmodat(host::AT_FDCWD, link.as_ptr(), mode as host::mode_t, 0) } != 0 {
            return Err(last());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        if mode_of(&stat_at(dir, name)?) & S_IFMT == S_IFLNK {
            return Err(EOPNOTSUPP);
        }
        // A symlink put at the name since has its own mode changed, not its target's.
        // SAFETY: fchmodat(2) of a NUL-terminated name in a directory we hold.
        if unsafe {
            libc::fchmodat(
                dir,
                name.as_ptr(),
                mode as host::mode_t,
                host::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(last());
        }
        Ok(())
    }
}

/// The user this process serves as, which owns what it makes (geteuid(2)), read once.
fn served_by() -> u32 {
    static EUID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: geteuid(2) takes nothing and cannot fail.
    *EUID.get_or_init(|| unsafe { libc::geteuid() })
}

/// Another descriptor of what `fd` is open to (fcntl(2) F_DUPFD_CLOEXEC).
fn dup_fd(fd: host::Raw) -> Result<host::Owned, Errno> {
    // SAFETY: fcntl(2) F_DUPFD_CLOEXEC of a descriptor the server holds open.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(last());
    }
    // SAFETY: a descriptor fcntl(2) just made, owned here alone.
    Ok(unsafe { host::Owned::from_raw_fd(copy) })
}

/// What a file system holds and has room for, as a guest's STATFS asks it.
struct FsStats {
    blocks: u64,
    free: u64,
    available: u64,
    files: u64,
    files_free: u64,
    block_size: u32,
    fragment_size: u32,
}

/// The file system `fd` is on's [`FsStats`] (fstatvfs(2)).
fn fs_stats(fd: host::Raw) -> Result<FsStats, Errno> {
    // SAFETY: an all-zero statvfs is a valid out-parameter.
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: fstatvfs(2) of a descriptor we hold.
    if unsafe { libc::fstatvfs(fd, &mut v) } != 0 {
        return Err(last());
    }
    #[allow(
        clippy::useless_conversion,
        clippy::unnecessary_cast,
        clippy::cast_possible_truncation
    )]
    Ok(FsStats {
        blocks: v.f_blocks as u64,
        free: v.f_bfree as u64,
        available: v.f_bavail as u64,
        files: v.f_files as u64,
        files_free: v.f_ffree as u64,
        block_size: v.f_bsize as u32,
        fragment_size: v.f_frsize as u32,
    })
}

/// Removes `fd`'s attribute `name` (fremovexattr(2)).
fn fremove_xattr(fd: host::Raw, name: &CStr) -> Result<(), Errno> {
    // SAFETY: fremovexattr(2) of a NUL-terminated name.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::fremovexattr(fd, name.as_ptr(), 0) };
    // SAFETY: as above.
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::fremovexattr(fd, name.as_ptr()) };
    if rc != 0 {
        return Err(last());
    }
    Ok(())
}

/// `fd`'s offset moved to `offset` from `whence` (this host's, `host::whence`): where it is
/// (lseek(2)).
fn seek(fd: host::Raw, offset: u64, whence: host::c_int) -> Result<u64, Errno> {
    #[allow(clippy::cast_possible_wrap)]
    // SAFETY: lseek(2) of a descriptor we hold.
    let at = unsafe { libc::lseek(fd, offset as host::off_t, whence) };
    u64::try_from(at).map_err(|_| last())
}

/// `fd`'s file cut or grown to `size` bytes (ftruncate(2)).
fn truncate_fd(fd: host::Raw, size: u64) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_wrap)]
    // SAFETY: ftruncate(2) of a descriptor we hold.
    if unsafe { libc::ftruncate(fd, size as host::off_t) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// `fd`'s file given the host's permissions `perm` (fchmod(2)).
fn chmod_fd(fd: host::Raw, perm: u32) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: fchmod(2) of a descriptor we hold.
    if unsafe { libc::fchmod(fd, perm as host::mode_t) } != 0 {
        return Err(last());
    }
    Ok(())
}

fn key(st: &Stat) -> (u64, u64) {
    (st.dev, st.ino)
}

fn is_dir(st: &Stat) -> bool {
    mode_of(st) & S_IFMT == S_IFDIR
}

/// A reply being built.
#[derive(Debug, Default)]
pub struct Reply(pub Vec<u8>);

impl Reply {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    /// struct fuse_attr; `dax` where the guest maps the file into its DAX window
    /// ([`Server::dax_of`]).
    fn attr(&mut self, st: &Stat, owner: Owner, dax: bool) -> &mut Self {
        let (size, blocks, at, mt, ct) = (st.size, st.blocks, st.atime, st.mtime, st.ctime);
        let host_mode = mode_of(st);
        let perm = owner.mode.unwrap_or(host_mode & 0o7777);
        // A node the host keeps as an empty regular file, as the guest made it.
        let emulated = owner.kind.filter(|_| host_mode & S_IFMT == S_IFREG);
        let mode = emulated.unwrap_or(host_mode & S_IFMT) | (perm & 0o7777);
        let (nlink, host_rdev, blksize) = (st.nlink, st.rdev, st.blksize);
        let rdev = match emulated {
            Some(_) => owner.rdev.unwrap_or(0),
            None => host::encode_dev(host_rdev),
        };
        let (dev, ino) = key(st);
        let _ = dev;
        self.u64(ino)
            .u64(size)
            .u64(blocks)
            .u64(at.0)
            .u64(mt.0)
            .u64(ct.0)
            .u32(at.1)
            .u32(mt.1)
            .u32(ct.1)
            .u32(mode)
            .u32(nlink)
            .u32(owner.uid)
            .u32(owner.gid)
            .u32(rdev)
            .u32(blksize)
            .u32(if emulated.is_none() && dax { ATTR_DAX } else { 0 })
    }
    /// struct fuse_entry_out.
    fn entry(&mut self, nodeid: u64, st: &Stat, owner: Owner, dax: bool) -> &mut Self {
        self.u64(nodeid)
            .u64(0)
            .u64(VALID_S)
            .u64(VALID_S)
            .u32(0)
            .u32(0)
            .attr(st, owner, dax)
    }
}

/// The host's limits on open descriptors, from which a share process's servers have what
/// they may hold between requests (audit V09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How many descriptors this process may have (`platform::descriptor_limit`), its limit
    /// raised as far as it may go first (`platform::raise_descriptor_limit`).
    pub process: u64,
    /// How many it has open, outside any server's: its standard streams, its connections,
    /// its shared directories.
    pub held: u64,
    /// The system's table of open files: how many it holds, and how many are open.
    pub table: Option<(u64, u64)>,
    /// How many processes the user may have.
    pub processes: Option<u64>,
}

/// The processes of a run that shares: its VM's and its share process.
const PROCESSES_A_RUN: u64 = 2;

/// Descriptors one request may hold past its server's room, at its most: a RENAME's two
/// directories, both let go, the first held while the second's path is walked a component at
/// a time, which holds two at once (one on Linux, where openat2 walks it), or a pin for what
/// it renames over once both are held. Nothing else holds more: a directory, its handle and
/// a file looked at, a handle opened again along its path, or a directory and two of a walk.
/// `tests/fs_dir_budget.rs` gives a server exactly this much past its room, at its room,
/// and a request that took more would be refused by the kernel.
pub const REQUEST_FDS: u64 = 3;

impl Limits {
    /// This process's, now; none if it cannot say how many descriptors it may have or has.
    pub fn now() -> Option<Limits> {
        Some(Limits {
            process: crate::platform::descriptor_limit()?,
            held: crate::platform::open_descriptors()?,
            table: crate::platform::file_table(),
            processes: crate::platform::user_processes(),
        })
    }

    /// What each of `servers` servers in this process may hold between requests: the
    /// process's room, less what it holds and what each server's requests take past their
    /// room; and no more than a share process's part of the system's table, as many share
    /// processes running as the user may start runs, so that they cannot fill it.
    pub fn budget(&self, servers: u64) -> u64 {
        let servers = servers.max(1);
        let mut room = self
            .process
            .saturating_sub(self.held)
            .saturating_sub(REQUEST_FDS.saturating_mul(servers));
        if let (Some((size, open)), Some(processes)) = (self.table, self.processes) {
            let runs = (processes / PROCESSES_A_RUN).max(1);
            room = room.min(size.saturating_sub(open) / runs);
        }
        room / servers
    }
}

impl Server {
    /// The server of the directory `root` holds, read-only or not, or of its one name
    /// `only`, alone in its process: it may hold what the process has room for.
    pub fn new(root: host::Owned, read_only: bool, only: Option<CString>) -> Result<Server, String> {
        let limits = Limits::now().ok_or("this process's limit on descriptors is unknown")?;
        Server::with_budget(root, read_only, only, limits.budget(1))
    }

    /// [`Server::new`], holding at most `budget` descriptors between requests.
    pub fn with_budget(
        root: host::Owned,
        read_only: bool,
        only: Option<CString>,
        budget: u64,
    ) -> Result<Server, String> {
        let st = stat_fd(root.raw()).map_err(|e| format!("the shared directory: errno {e}"))?;
        if !is_dir(&st) {
            return Err("the shared directory is not a directory".into());
        }
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT,
            Node {
                dir: true,
                parent: ROOT,
                name: CString::default(),
                links: Vec::new(),
                lookups: 1,
                key: key(&st),
                volume: NO_VOLUME,
            },
        );
        let mut by_key = HashMap::new();
        let (dev, ino) = key(&st);
        by_key.insert((NO_VOLUME, dev, ino), ROOT);
        Ok(Server {
            read_only,
            only,
            state: Mutex::new(State {
                root,
                nodes,
                by_key,
                joined: false,
                volumes: HashMap::new(),
                named: HashMap::new(),
                next_node: 0,
                ids: std::hash::RandomState::new(),
                handles: HashMap::new(),
                next_handle: 1,
                opened: HashMap::new(),
                fds: Fds::default(),
                pins: HashMap::new(),
                unreached: HashMap::new(),
                budget,
                ghosts: None,
                headroom: None,
            }),
        })
    }

    /// A join share's server (D119): its root the empty directory `root` holds, nothing
    /// made there, and each volume [`add`](Self::add)ed reached from it by its name.
    pub fn joined(root: host::Owned, budget: u64) -> Result<Server, String> {
        let server = Server::with_budget(root, false, None, budget)?;
        server.state.lock().unwrap_or_else(PoisonError::into_inner).joined = true;
        Ok(server)
    }

    /// Adds volume `name` to a join share: the directory `dir` holds, read-only or not, or
    /// of its one name `only`, a root of its own under the share's.
    pub fn add(
        &self,
        name: CString,
        dir: host::Owned,
        read_only: bool,
        only: Option<CString>,
    ) -> Result<(), String> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
            return Err(format!("a volume named {name:?}"));
        }
        let st = stat_fd(dir.raw()).map_err(|e| format!("volume {name:?}: errno {e}"))?;
        if !is_dir(&st) {
            return Err(format!("volume {name:?} is not a directory"));
        }
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !s.joined {
            return Err("a share of one directory takes no volumes".into());
        }
        if s.named.contains_key(&name) {
            return Err(format!("volume {name:?} is served already"));
        }
        let id = fresh_node(&mut s);
        let (dev, ino) = key(&st);
        s.nodes.insert(
            id,
            Node {
                dir: true,
                parent: ROOT,
                name: name.clone(),
                links: Vec::new(),
                lookups: 0,
                key: (dev, ino),
                volume: id,
            },
        );
        s.by_key.insert((id, dev, ino), id);
        s.volumes.insert(
            id,
            Volume {
                fd: dir,
                read_only,
                only,
            },
        );
        s.named.insert(name, id);
        Ok(())
    }

    /// Takes volume `name` out of a join share, its directory let go of: what the guest
    /// still names of it is stale, and none of the share's paths leads to it.
    pub fn remove(&self, name: &CStr) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = s.named.remove(name) {
            s.volumes.remove(&id);
        }
    }

    /// What a snapshot keeps of the share (D176): each node the guest's kernel holds, by
    /// its ID, its parent and name, its other names, its count of lookups and what it is;
    /// each handle by its ID, its node and its flags; and how many of each node's handles
    /// read and write. Its descriptors it keeps none of: a restored server opens each along
    /// its node's path, checked to be the node still, as it opens one it let go of. Asked
    /// between requests, the share's device paused. Refused where a node is open that no
    /// path reaches: one the guest removed the last name of, which no restore could open.
    pub fn save(&self) -> Result<Vec<u8>, String> {
        let s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !s.unreached.is_empty() {
            return Err(
                "a file the microVM has open was removed from its volume with no descriptor left to keep it by"
                    .into(),
            );
        }
        // The files the guest holds open that no path reaches, each by its pin: their bytes,
        // which a restored server holds anew, as CRIU's images carry a removed file's
        // ("ghost files"), within what a snapshot's state holds.
        let mut ghosts = Vec::with_capacity(s.pins.len());
        let mut carried = 0u64;
        for (node, (fd, access)) in &s.pins {
            let st =
                stat_fd(fd.raw()).map_err(|e| format!("a removed file the microVM has open: errno {e}"))?;
            if mode_of(&st) & S_IFMT != S_IFREG {
                return Err(
                    "a directory or special file the microVM has open was removed from its volume".into(),
                );
            }
            #[allow(clippy::cast_sign_loss)]
            let size = st.size;
            carried = carried.saturating_add(size);
            if carried > MAX_TABLES as u64 {
                return Err(format!(
                    "files the microVM has open after removing them from its volume hold {carried} bytes or more, \
                     more than a snapshot keeps ({MAX_TABLES})"
                ));
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(usize::try_from(size).map_err(|_| "a removed file too large to keep")?)
                .map_err(|e| format!("a removed file's {size} bytes: {e}"))?;
            bytes.resize(bytes.capacity(), 0);
            let file = std::fs::File::from(fd.try_clone().map_err(|e| format!("a removed file: {e}"))?);
            crate::platform::read_exact_at(&file, &mut bytes, 0)
                .map_err(|e| format!("a removed file's bytes: {e}"))?;
            ghosts.push((*node, *access, bytes));
        }
        ghosts.sort_unstable_by_key(|(node, _, _)| *node);
        if !s.volumes.is_empty() {
            return Err("a joined container's volume is shared with the microVM".into());
        }
        let root = s.nodes.get(&ROOT).ok_or("the share has no root")?;
        let mut w = Writer::default();
        w.bool(s.joined);
        w.u64(root.key.0);
        w.u64(s.next_node);
        w.u64(s.next_handle);
        let mut nodes: Vec<(&u64, &Node)> = s.nodes.iter().collect();
        nodes.sort_unstable_by_key(|(id, _)| **id);
        w.seq(&nodes, |w, (id, n)| {
            w.u64(**id);
            w.bool(n.dir);
            w.u64(n.parent);
            w.bytes(n.name.as_bytes());
            w.seq(&n.links, |w, (dir, name)| {
                w.u64(*dir);
                w.bytes(name.as_bytes());
            });
            w.u64(n.lookups);
            w.u64(n.key.0);
            w.u64(n.key.1);
            w.u64(n.volume);
        });
        let mut handles: Vec<(&u64, &Handle)> = s.handles.iter().collect();
        handles.sort_unstable_by_key(|(id, _)| **id);
        w.seq(&handles, |w, (id, h)| {
            w.u64(**id);
            w.u64(h.node);
            w.bool(h.flags.is_some());
            #[allow(clippy::cast_sign_loss)]
            w.u32(h.flags.unwrap_or(0) as u32);
        });
        let mut opened: Vec<(&u64, &Opened)> = s.opened.iter().collect();
        opened.sort_unstable_by_key(|(id, _)| **id);
        w.seq(&opened, |w, (id, o)| {
            w.u64(**id);
            w.u64(o.handles);
            w.u64(o.reading);
            w.u64(o.writing);
        });
        w.seq(&ghosts, |w, (node, access, bytes)| {
            w.u64(*node);
            #[allow(clippy::cast_sign_loss)]
            w.u32(*access as u32);
            w.bytes(bytes);
        });
        Ok(w.into_bytes())
    }

    /// Makes the files its guest held open that no path reaches in `dir`, as it is restored
    /// from a save (D176), each removed as soon as it is made.
    pub fn keep_ghosts_in(&self, dir: host::Owned) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).ghosts = Some(dir);
    }

    /// Drops its host's copy of what the guest reads once keeping it would leave the budgets
    /// `headroom` watches less room than the share's own reads need (D188): the guest keeps
    /// what it read in its own page cache.
    pub fn drop_reads_past(&self, headroom: super::headroom::Headroom) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).headroom = Some(headroom);
    }

    /// Takes the share as a snapshot kept it ([`save`](Self::save)), into a server no guest
    /// has spoken to: each node and handle the guest holds, its descriptors opened again as
    /// it asks. The shared directory is to be the one saved: a node is the file it was by
    /// its device and inode, so where the directory's device numbers changed, as a host's
    /// boot may change them, the nodes of its own device are taken as of the new one; a
    /// directory that is another (its inode is not the saved root's) is refused, as every
    /// node the guest holds would be stale.
    pub fn restore(&self, table: &[u8]) -> Result<(), String> {
        let bad = |e: crate::snapshot::codec::DecodeError| format!("a share's saved tables: {}", e.0);
        let mut r = Reader::new(table);
        let joined = r.bool().map_err(bad)?;
        let saved_dev = r.u64().map_err(bad)?;
        let next_node = r.u64().map_err(bad)?;
        let next_handle = r.u64().map_err(bad)?;
        let name = |r: &mut Reader<'_>| -> crate::snapshot::codec::Result<CString> {
            let bytes = r.bytes(host::PATH_MAX as usize)?;
            CString::new(bytes).map_err(|_| crate::snapshot::codec::DecodeError("a name with a NUL".into()))
        };
        let nodes = r
            .seq(u32::MAX as usize, 8 * 6 + 1 + 4 + 4, |r| {
                let id = r.u64()?;
                let dir = r.bool()?;
                let parent = r.u64()?;
                let own = name(r)?;
                let links = r.seq(u32::MAX as usize, 8 + 4, |r| Ok((r.u64()?, name(r)?)))?;
                Ok((
                    id,
                    Node {
                        dir,
                        parent,
                        name: own,
                        links,
                        lookups: r.u64()?,
                        key: (r.u64()?, r.u64()?),
                        volume: r.u64()?,
                    },
                ))
            })
            .map_err(bad)?;
        let handles = r
            .seq(u32::MAX as usize, 8 + 8 + 1 + 4, |r| {
                let id = r.u64()?;
                let node = r.u64()?;
                let has = r.bool()?;
                #[allow(clippy::cast_possible_wrap)]
                let flags = r.u32()? as host::c_int;
                Ok((
                    id,
                    Handle {
                        node,
                        flags: has.then_some(flags),
                    },
                ))
            })
            .map_err(bad)?;
        let opened = r
            .seq(u32::MAX as usize, 8 * 4, |r| {
                Ok((
                    r.u64()?,
                    Opened {
                        handles: r.u64()?,
                        reading: r.u64()?,
                        writing: r.u64()?,
                    },
                ))
            })
            .map_err(bad)?;
        let ghosts = r
            .seq(u32::MAX as usize, 8 + 4 + 4, |r| {
                let node = r.u64()?;
                #[allow(clippy::cast_possible_wrap)]
                let access = r.u32()? as host::c_int;
                Ok((node, access, r.bytes(MAX_TABLES)?))
            })
            .map_err(bad)?;
        r.finish().map_err(bad)?;
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if s.joined != joined {
            return Err("a share's saved tables are another kind of share's".into());
        }
        if s.nodes.len() != 1 || !s.handles.is_empty() {
            return Err("a share's saved tables, for a server the guest has spoken to already".into());
        }
        let now = s.nodes.get(&ROOT).map(|n| n.key).ok_or("the share has no root")?;
        let mut nodes: HashMap<u64, Node> = nodes.into_iter().collect();
        let root = nodes.get(&ROOT).ok_or("a share's saved tables have no root")?;
        if root.key.1 != now.1 {
            return Err(
                "its volume is not the directory it was saved with: every file the microVM holds of it would be stale"
                    .into(),
            );
        }
        for n in nodes.values_mut() {
            if n.key.0 == saved_dev {
                n.key.0 = now.0;
            }
        }
        let known = |id: &u64| nodes.contains_key(id);
        if nodes.iter().any(|(id, n)| {
            *id == NO_VOLUME
                || !known(&n.parent)
                || (*id != ROOT && (n.name.as_bytes().is_empty() || n.name.as_bytes().contains(&b'/')))
                || (n.volume != NO_VOLUME && !known(&n.volume))
                || n.links.iter().any(|(dir, _)| !known(dir))
        }) || handles.iter().any(|(_, h)| !known(&h.node))
            || opened.iter().any(|(id, _)| !known(id))
            || ghosts.iter().any(|(id, _, _)| !known(id))
        {
            return Err("a share's saved tables name a node they do not hold".into());
        }
        let mut pins = HashMap::with_capacity(ghosts.len());
        if !ghosts.is_empty() {
            let dir = s.ghosts.as_ref().ok_or(
                "the files its guest holds that no path reaches, for a share with nowhere to keep them",
            )?;
            for (node, access, bytes) in ghosts {
                pins.insert(node, (ghost(dir.raw(), bytes)?, access));
            }
        }
        let mut by_key = HashMap::with_capacity(nodes.len());
        for (id, n) in &nodes {
            by_key.insert((n.volume, n.key.0, n.key.1), *id);
        }
        s.nodes = nodes;
        s.by_key = by_key;
        s.next_node = next_node;
        s.handles = handles.into_iter().collect();
        s.next_handle = next_handle;
        s.opened = opened.into_iter().collect();
        s.fds = Fds::default();
        s.pins = pins;
        Ok(())
    }

    /// Whether the guest is told to map node `node`'s file, `st`, into its DAX window
    /// (D189): a regular file of at least [`DAX_SMALLEST`] the server may read, as a
    /// mapping does; and write, in a volume the guest may write, wherever a mapping's
    /// descriptor is opened to write too (window::SHARED_NEEDS_WRITE).
    fn dax_of(&self, s: &State, node: u64, st: &Stat) -> bool {
        let euid = served_by();
        let mode = mode_of(st);
        let size = st.size;
        // The owner's bits where the server owns it, else others': a group's members are
        // not looked up, and a file only a group gives is read through FUSE.
        #[allow(clippy::unnecessary_cast)]
        let (read, write) = if st.uid == euid as u32 {
            (host::S_IRUSR as u32, host::S_IWUSR as u32)
        } else {
            (host::S_IROTH as u32, host::S_IWOTH as u32)
        };
        let large = mode & S_IFMT == S_IFREG && size >= DAX_SMALLEST;
        let reachable = mode & read != 0
            && (!super::window::SHARED_NEEDS_WRITE || (mode & write != 0 && self.writable(s, node).is_ok()));
        large && reachable
    }

    /// A descriptor of node `node`'s file for a DAX window's mapping of it (D189), opened
    /// to read, and to write too where `writable`: the guest names a mapping's file by its
    /// node alone, its handle -1 (fs/fuse/dax.c, `fuse_setup_one_mapping`), so the server
    /// opens it, as virtiofsd does (`setupmapping`). Along its path, checked to be the node;
    /// a file no path reaches, by a copy of its pin where that allows as much. A Linux
    /// errno where it gives none.
    pub fn descriptor(&self, node: u64, writable: bool) -> Result<host::Owned, Errno> {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let s = &mut *s;
        let n = s.nodes.get(&node).ok_or(ENOENT)?;
        if n.dir {
            return Err(EISDIR);
        }
        let want = n.key;
        // Opened to write too wherever the hypervisor maps a shared mapping only of such a
        // descriptor (window::SHARED_NEEDS_WRITE): never in a volume the guest may not write.
        let rdwr = writable || super::window::SHARED_NEEDS_WRITE;
        if rdwr {
            self.writable(s, node)?;
        }
        let access = if rdwr { host::O_RDWR } else { host::O_RDONLY };
        let flags = access | host::O_NOFOLLOW | host::O_CLOEXEC;
        let why = match at(s, node).and_then(|(dir, name)| open_regular(dir, &name, flags, 0)) {
            Ok((fd, st)) if key(&st) == want => return Ok(fd),
            Ok(_) => ESTALE,
            Err(e) => stale(e),
        };
        let pin = match s.pins.get(&node) {
            Some((pin, held)) if why == ESTALE && allows(*held, access) => pin,
            _ => return Err(why),
        };
        dup_fd(pin.raw())
    }

    /// Answers request `req`, the bytes of its readable descriptors: the reply's bytes,
    /// none for a request that takes none (FORGET, BATCH_FORGET, INTERRUPT).
    pub fn handle(&self, req: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.handle_into(req, &mut out).then_some(out)
    }

    /// [`handle`](Self::handle) into `out`, which it empties and leaves the reply in: a
    /// buffer its caller keeps from one request to the next, so that a reply is built
    /// where it is sent from, a READ's bytes read into it. False for a request that takes
    /// no reply. Each MiB read through the share otherwise made four MiB of fresh memory,
    /// its server's buffer, its reply, its frame and the frame's copy, whose host held its
    /// run's cgroup at its budget as it read (PM M314).
    pub fn handle_into(&self, req: &[u8], out: &mut Vec<u8>) -> bool {
        let mut a = Args(req);
        let header = (|| -> Result<_, Errno> {
            let len = a.u32()?;
            let opcode = a.u32()?;
            let unique = a.u64()?;
            let nodeid = a.u64()?;
            let uid = a.u32()?;
            let gid = a.u32()?;
            let _pid = a.u32()?;
            let _ext = a.u32()?;
            Ok((len, opcode, unique, nodeid, uid, gid))
        })();
        let Ok((len, opcode, unique, nodeid, uid, gid)) = header else {
            return false;
        };
        let body = req
            .get(40..usize::try_from(len).unwrap_or(0).min(req.len()))
            .unwrap_or_default();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let caller = (uid, gid);
        // Its out header's room first, filled once the answer is known.
        let mut r = Reply(std::mem::take(out));
        r.0.clear();
        r.0.resize(OUT_HEADER_BYTES, 0);
        let answer = match opcode {
            op::FORGET => {
                let n = Args(body).u64().unwrap_or(0);
                forget(&mut state, nodeid, n);
                *out = r.0;
                return false;
            }
            op::BATCH_FORGET => {
                let mut b = Args(body);
                let count = b.u32().unwrap_or(0);
                let _ = b.u32();
                for _ in 0..count {
                    let (Ok(n), Ok(l)) = (b.u64(), b.u64()) else {
                        break;
                    };
                    forget(&mut state, n, l);
                }
                *out = r.0;
                return false;
            }
            op::INTERRUPT => {
                *out = r.0;
                return false;
            }
            _ => match self.confined(&state, opcode, nodeid, body) {
                Ok(()) => self.dispatch(&mut state, opcode, nodeid, caller, body, &mut r),
                Err(e) => Err(e),
            },
        };
        // Between requests, the descriptors held fit the room the pins leave.
        let pins = u64::try_from(state.pins.len()).unwrap_or(u64::MAX);
        let room = state.budget.saturating_sub(pins);
        state.fds.trim(room);
        let error = match answer {
            Ok(()) => 0i32,
            Err(e) => {
                r.0.truncate(OUT_HEADER_BYTES);
                -e
            }
        };
        let total = u32::try_from(r.0.len()).unwrap_or(u32::MAX);
        let mut header = [0u8; OUT_HEADER_BYTES];
        let (len_bytes, rest) = header.split_at_mut(4);
        let (error_bytes, unique_bytes) = rest.split_at_mut(4);
        len_bytes.copy_from_slice(&total.to_le_bytes());
        error_bytes.copy_from_slice(&error.to_le_bytes());
        unique_bytes.copy_from_slice(&unique.to_le_bytes());
        if let Some(at) = r.0.get_mut(..OUT_HEADER_BYTES) {
            at.copy_from_slice(&header);
        }
        *out = r.0;
        true
    }

    /// Whether the share lets the request through: a file bound alone, at its directory,
    /// only the file's lookup and what reads the directory itself, no name moving in or out,
    /// as none does of a file bind mount (EBUSY); a join share, as [`joined_confined`] says.
    fn confined(&self, s: &State, opcode: u32, nodeid: u64, body: &[u8]) -> Result<(), Errno> {
        if s.joined {
            return joined_confined(s, opcode, nodeid, body);
        }
        match &self.only {
            Some(only) => only_confined(only, ROOT, opcode, nodeid, body),
            None => Ok(()),
        }
    }

    /// Whether the request may change node `nodeid`: in a share written to, and in a join
    /// share's volume written to, never its root.
    fn writable(&self, s: &State, nodeid: u64) -> Result<(), Errno> {
        if self.read_only {
            return Err(EROFS);
        }
        if !s.joined {
            return Ok(());
        }
        let volume = s.nodes.get(&nodeid).map_or(NO_VOLUME, |n| n.volume);
        match s.volumes.get(&volume) {
            Some(v) if !v.read_only => Ok(()),
            _ => Err(EROFS),
        }
    }

    fn dispatch(
        &self,
        s: &mut State,
        opcode: u32,
        nodeid: u64,
        caller: (u32, u32),
        body: &[u8],
        r: &mut Reply,
    ) -> Result<(), Errno> {
        let mut a = Args(body);
        match opcode {
            op::INIT => {
                // A session begins: nothing the guest knew before it holds.
                reset(s);
                let major = a.u32()?;
                let minor = a.u32()?;
                let readahead = a.u32()?;
                let flags = a.u32()?;
                let flags2 = if flags & init::INIT_EXT != 0 {
                    a.u32().unwrap_or(0)
                } else {
                    0
                };
                if major != 7 {
                    // The kernel retries with ours.
                    r.u32(7).u32(45);
                    return Ok(());
                }
                let want = init::ASYNC_READ
                    | init::ATOMIC_O_TRUNC
                    | init::BIG_WRITES
                    | init::DO_READDIRPLUS
                    | init::READDIRPLUS_AUTO
                    | init::PARALLEL_DIROPS
                    | init::MAX_PAGES
                    | init::CACHE_SYMLINKS
                    | init::MAP_ALIGNMENT
                    | init::INIT_EXT;
                // A DAX window's ranges are whole pages of this host's (D189).
                let page = crate::platform::page_size().unwrap_or(4096);
                let alignment = if flags & init::MAP_ALIGNMENT != 0 {
                    u16::try_from(u64::try_from(page).unwrap_or(4096).trailing_zeros()).unwrap_or(12)
                } else {
                    0
                };
                r.u32(7)
                    .u32(minor.min(45))
                    .u32(readahead)
                    .u32(flags & want)
                    .u16(16)
                    .u16(12)
                    .u32(MAX_WRITE)
                    .u32(1)
                    .u16(MAX_PAGES)
                    .u16(alignment)
                    .u32(flags2 & init::HAS_INODE_DAX)
                    .u32(0)
                    .u16(0)
                    .bytes(&[0u8; 22]);
            }
            op::DESTROY => reset(s),
            op::LOOKUP => {
                let name = a.name()?;
                let (id, st, owner) = lookup(s, nodeid, &name)?;
                let dax = self.dax_of(s, id, &st);
                r.entry(id, &st, owner, dax);
            }
            op::GETATTR => {
                let flags = a.u32()?;
                let _ = a.u32()?;
                let fh = a.u64()?;
                // The node's own stat, or an open file's, which the guest names and need
                // not be the node's: only the node's stands for its type.
                let (st, own) = if flags & 1 != 0 && is_file(s, fh) {
                    (stat_fd(file_fd(s, fh)?)?, false)
                } else {
                    (node_stat(s, nodeid)?, true)
                };
                let owner = node_owner(s, nodeid, own.then_some(&st))?;
                let dax = self.dax_of(s, nodeid, &st);
                r.u64(VALID_S).u32(0).u32(0).attr(&st, owner, dax);
            }
            op::SETATTR => {
                self.writable(s, nodeid)?;
                let valid = Args(body).u32()?;
                let fh = Args(body.get(8..).unwrap_or_default()).u64()?;
                self.setattr(s, nodeid, &mut a)?;
                // An open file's, where the guest names one: one unlinked has no path.
                let (st, own) = if valid & fattr::FH != 0 && is_file(s, fh) {
                    (stat_fd(file_fd(s, fh)?)?, false)
                } else {
                    (node_stat(s, nodeid)?, true)
                };
                let owner = node_owner(s, nodeid, own.then_some(&st))?;
                let dax = self.dax_of(s, nodeid, &st);
                r.u64(VALID_S).u32(0).u32(0).attr(&st, owner, dax);
            }
            op::READLINK => {
                let (dir, name) = at(s, nodeid)?;
                r.bytes(&readlink_at(dir, &name)?);
            }
            op::SYMLINK => {
                self.writable(s, nodeid)?;
                let name = a.name()?;
                let target = a.cstr()?;
                let dir = dir_fd(s, nodeid)?;
                symlink_at(&target, dir, &name)?;
                self.made(s, nodeid, &name, caller, Kept::default(), r)?;
            }
            op::MKNOD => {
                self.writable(s, nodeid)?;
                let mode = a.u32()?;
                let rdev = a.u32()?;
                let _umask = a.u32()?;
                let _ = a.u32()?;
                let name = a.name()?;
                let dir = dir_fd(s, nodeid)?;
                let kind = mode & S_IFMT;
                let perm = mode & 0o7777;
                let empty = |perm: u32| {
                    open_at(
                        dir,
                        &name,
                        host::O_CREAT | host::O_EXCL | host::O_WRONLY,
                        host_perm(perm, false),
                    )
                    .map(drop)
                };
                let kept = if kind == S_IFREG || kind == 0 {
                    empty(perm)?;
                    Kept::mode(perm)
                } else {
                    // A device node takes the guest's mode as it is: this process never
                    // opens one (the guest's kernel opens a device itself), and none can hold
                    // the owner's attribute (`open_meta`), so the owner's access a FIFO or a
                    // socket keeps would only misstate its mode.
                    let on_host = if matches!(kind, S_IFCHR | S_IFBLK) {
                        perm
                    } else {
                        host_perm(perm, false)
                    };
                    match mknod_at(dir, &name, kind | on_host, rdev) {
                        Ok(()) => Kept::mode(perm),
                        // A node the host makes only for a privileged process (a device,
                        // the whiteout overlayfs marks a removal with: one of 0/0): an
                        // empty regular file, the guest's kind and number kept beside it,
                        // as the guest sees it. The guest's kernel opens a device node
                        // itself, never through the share.
                        Err(EPERM) => {
                            empty(0o600)?;
                            Kept {
                                mode: Some(perm),
                                kind: Some(kind),
                                rdev: Some(rdev),
                            }
                        }
                        Err(e) => return Err(e),
                    }
                };
                self.made(s, nodeid, &name, caller, kept, r)?;
            }
            op::MKDIR => {
                self.writable(s, nodeid)?;
                let mode = a.u32()?;
                let _umask = a.u32()?;
                let name = a.name()?;
                let dir = dir_fd(s, nodeid)?;
                mkdir_at(dir, &name, host_perm(mode, true))?;
                self.made(s, nodeid, &name, caller, Kept::mode(mode), r)?;
            }
            op::UNLINK | op::RMDIR => {
                self.writable(s, nodeid)?;
                let name = a.name()?;
                let dir = dir_fd(s, nodeid)?;
                let removal = pin_before_removal(s, volume_of(s, nodeid), dir, &name);
                let failed = unlink_at(dir, &name, opcode == op::RMDIR).err();
                removed(s, removal, failed.is_none());
                if let Some(e) = failed {
                    return Err(e);
                }
            }
            op::RENAME | op::RENAME2 => {
                self.writable(s, nodeid)?;
                let newdir = a.u64()?;
                // Two volumes are two mounts in the guest, as two of Docker's binds are:
                // nothing moves between them.
                if volume_of(s, nodeid) != volume_of(s, newdir) {
                    return Err(EXDEV);
                }
                self.writable(s, newdir)?;
                let flags = if opcode == op::RENAME2 {
                    let f = a.u32()?;
                    let _ = a.u32()?;
                    f
                } else {
                    0
                };
                let old = a.name()?;
                let new = a.name()?;
                if flags & linux::RENAME_WHITEOUT == 0 {
                    rename(s, nodeid, &old, newdir, &new, flags)?;
                } else {
                    self.rename_whiteout(s, nodeid, &old, newdir, &new, flags, caller)?;
                }
            }
            op::LINK => {
                self.writable(s, nodeid)?;
                let old = a.u64()?;
                if volume_of(s, old) != volume_of(s, nodeid) {
                    return Err(EXDEV);
                }
                let name = a.name()?;
                let (odir, oname) = at(s, old)?;
                let dir = dir_fd(s, nodeid)?;
                link_at(odir, &oname, dir, &name)?;
                let (id, st, owner) = lookup(s, nodeid, &name)?;
                let dax = self.dax_of(s, id, &st);
                r.entry(id, &st, owner, dax);
            }
            op::OPEN => {
                let flags = a.u32()?;
                if flags & linux::O_ACCMODE != 0 || flags & linux::O_TRUNC != 0 {
                    self.writable(s, nodeid)?;
                }
                if s.nodes.get(&nodeid).is_some_and(|n| n.dir) {
                    return Err(EISDIR);
                }
                room_for_handle(s)?;
                let flags = host::open_flags(flags & !linux::O_CREAT);
                let want = s.nodes.get(&nodeid).ok_or(ENOENT)?.key;
                // Along its path, checked to be the node: replaced on the host since the guest
                // looked it up, its kernel looks again (ESTALE).
                let opened = match at(s, nodeid).and_then(|(dir, name)| open_regular(dir, &name, flags, 0)) {
                    Ok((fd, st)) if key(&st) == want => Ok(fd),
                    Ok(_) => Err(ESTALE),
                    Err(e) => Err(stale(e)),
                };
                let held = match opened {
                    Ok(fd) => Some(Held::Fd(fd)),
                    // Unlinked or renamed over, open still: another handle on its pin, where
                    // that allows as much (a guest's open of /proc/self/fd).
                    Err(ESTALE) => match s.pins.get(&nodeid) {
                        Some((_, access)) if allows(*access, flags) => None,
                        _ => return Err(ESTALE),
                    },
                    Err(e) => return Err(e),
                };
                let fh = add_handle(s, nodeid, Some(once(flags)), held);
                r.u64(fh).u32(0).u32(0);
            }
            op::CREATE => {
                self.writable(s, nodeid)?;
                let flags = a.u32()?;
                let mode = a.u32()?;
                let _umask = a.u32()?;
                let _ = a.u32()?;
                let name = a.name()?;
                room_for_handle(s)?;
                let dir = dir_fd(s, nodeid)?;
                let flags = host::open_flags(flags) | host::O_CREAT;
                // Made here, or found there: a file O_CREAT finds keeps its owner and mode.
                let (fd, st, created) =
                    match open_regular(dir, &name, flags | host::O_EXCL, host_perm(mode, false)) {
                        Ok((fd, st)) => (fd, st, true),
                        Err(EEXIST) if flags & host::O_EXCL == 0 => {
                            let (fd, st) = open_regular(dir, &name, flags & !host::O_CREAT, 0)?;
                            (fd, st, false)
                        }
                        Err(e) => return Err(e),
                    };
                let made = if created {
                    self.made(s, nodeid, &name, caller, Kept::mode(mode), r)?
                } else {
                    let (id, found, owner) = lookup(s, nodeid, &name)?;
                    let dax = self.dax_of(s, id, &found);
                    r.entry(id, &found, owner, dax);
                    id
                };
                // Made with a mode that refuses its own handle (a read-only file opened to be
                // filled, as tar, cp and git make one): none could be opened for it again, so
                // this descriptor is its pin.
                let held = if refuses(mode_of(&st), flags)
                    && s.nodes.get(&made).is_some_and(|n| n.key == key(&st))
                    && !s.pins.contains_key(&made)
                    && room_for_pin(s)
                {
                    s.pins.insert(made, (fd, flags & host::O_ACCMODE));
                    None
                } else {
                    Some(Held::Fd(fd))
                };
                let fh = add_handle(s, made, Some(once(flags)), held);
                r.u64(fh).u32(0).u32(0);
            }
            op::READ => {
                let fh = a.u64()?;
                let offset = a.u64()?;
                let size = a.u32()?.min(MAX_WRITE);
                let fd = file_fd(s, fh)?;
                // Read where the reply is sent from: no buffer of its own, nor a copy.
                let at = r.0.len();
                r.0.resize(at.saturating_add(size as usize), 0);
                let n = pread(fd, r.0.get_mut(at..).unwrap_or_default(), offset)?;
                r.0.truncate(at.saturating_add(n));
                if let Some(headroom) = s.headroom.as_mut()
                    && !headroom.fits(n as u64)
                {
                    drop_cached(fd, offset, n);
                }
            }
            op::WRITE => {
                self.writable(s, nodeid)?;
                let fh = a.u64()?;
                let offset = a.u64()?;
                let size = a.u32()?;
                let _flags = a.u32()?;
                let _owner = a.u64()?;
                let _ = a.u32()?;
                let _ = a.u32()?;
                let data = a.rest().get(..size as usize).ok_or(EINVAL)?;
                let fd = file_fd(s, fh)?;
                let n = pwrite(fd, data, offset)?;
                r.u32(u32::try_from(n).unwrap_or(0)).u32(0);
            }
            op::STATFS => {
                // A join share's volume's own file system (D119).
                let volume = volume_of(s, nodeid);
                let top = if s.volumes.contains_key(&volume) {
                    volume
                } else {
                    ROOT
                };
                let v = fs_stats(dir_fd(s, top)?)?;
                r.u64(v.blocks)
                    .u64(v.free)
                    .u64(v.available)
                    .u64(v.files)
                    .u64(v.files_free)
                    .u32(v.block_size)
                    .u32(255)
                    .u32(v.fragment_size)
                    .u32(0)
                    .bytes(&[0u8; 24]);
            }
            op::RELEASE | op::RELEASEDIR => {
                let fh = a.u64()?;
                release(s, fh);
            }
            op::FLUSH => {}
            op::FSYNC | op::FSYNCDIR => {
                let fh = a.u64()?;
                let pinned = s.handles.get(&fh).and_then(|h| s.pins.get(&h.node));
                let fd = match pinned {
                    Some((pin, _)) => pin.raw(),
                    None if is_file(s, fh) => file_fd(s, fh)?,
                    None => dir_fd(s, nodeid)?,
                };
                durable(fd)?;
            }
            op::SYNCFS => {}
            op::OPENDIR => {
                room_for_handle(s)?;
                let dir = dir_fd(s, nodeid)?;
                let stream = DirStream::open(dir)?;
                let fh = add_handle(s, nodeid, None, Some(Held::Dir(stream)));
                r.u64(fh).u32(0).u32(0);
            }
            op::READDIR | op::READDIRPLUS => {
                let fh = a.u64()?;
                let offset = a.u64()?;
                // At most a largest read, as INIT told the guest: a larger reply would not
                // fit the frame the device takes (audit V06).
                let size = (a.u32()? as usize).min(MAX_WRITE as usize);
                let plus = opcode == op::READDIRPLUS;
                let base = if plus { 152 } else { 24 };
                // A file bound alone: its directory shows it alone; and a join share's root
                // nothing of the directory it is, its volumes found by name alone (D119).
                let only: Option<CString> = if s.joined {
                    if nodeid == ROOT {
                        Some(CString::default())
                    } else {
                        s.volumes.get(&nodeid).and_then(|v| v.only.clone())
                    }
                } else {
                    self.only.clone().filter(|_| nodeid == ROOT)
                };
                let dir = dir_stream(s, fh)?;
                // The entries that fit, each with the offset after it, looked up (PLUS)
                // once the handle is let go.
                let mut fits = Vec::new();
                let mut used = 0;
                dir.read(offset, size, |entry, next| {
                    let bytes = entry.name.to_bytes();
                    if only
                        .as_ref()
                        .is_some_and(|only| !matches!(bytes, b"." | b"..") && entry.name != *only)
                    {
                        return true;
                    }
                    let len = (base + bytes.len() + 7) & !7;
                    if used + len > size {
                        return false;
                    }
                    used += len;
                    fits.push((entry.clone(), next));
                    true
                })?;
                for (entry, next) in fits {
                    let bytes = entry.name.to_bytes();
                    let mut kind = entry.kind;
                    if plus {
                        if bytes == b"." || bytes == b".." {
                            r.bytes(&[0u8; 128]);
                        } else {
                            match lookup(s, nodeid, &entry.name) {
                                Ok((id, st, owner)) => {
                                    if let Some(k) = owner.kind.filter(|_| mode_of(&st) & S_IFMT == S_IFREG) {
                                        kind = dirent_kind(k);
                                    }
                                    let dax = self.dax_of(s, id, &st);
                                    r.entry(id, &st, owner, dax);
                                }
                                // Gone since the directory was read: an entry the guest
                                // looks up, and does not find.
                                Err(_) => {
                                    r.bytes(&[0u8; 128]);
                                }
                            }
                        }
                    } else if kind == u32::from(host::DT_REG)
                        && let Some(k) = emulated_at(dir_fd(s, nodeid)?, &entry.name)
                    {
                        // Its kind, as overlayfs reads a whiteout's from a listing.
                        kind = dirent_kind(k);
                    }
                    r.u64(entry.ino)
                        .u64(next)
                        .u32(u32::try_from(bytes.len()).unwrap_or(0))
                        .u32(kind)
                        .bytes(bytes);
                    let pad = ((base + bytes.len() + 7) & !7) - base - bytes.len();
                    r.bytes([0u8; 8].get(..pad).unwrap_or_default());
                }
            }
            op::ACCESS => {}
            op::GETXATTR => {
                let size = a.u32()?;
                let _ = a.u32()?;
                let name = a.cstr()?;
                if hidden(&name) {
                    return Err(ENODATA);
                }
                let host_name = host_xattr(&name).map_err(|_| ENODATA)?;
                let fd = node_fd(s, nodeid)?;
                let value = fget_xattr(fd.raw(), &host_name)?.ok_or(ENODATA)?;
                if size == 0 {
                    r.u32(u32::try_from(value.len()).unwrap_or(u32::MAX)).u32(0);
                } else if value.len() > size as usize {
                    return Err(ERANGE);
                } else {
                    r.bytes(&value);
                }
            }
            op::LISTXATTR => {
                let size = a.u32()?;
                let _ = a.u32()?;
                let names = match node_fd(s, nodeid) {
                    Ok(fd) => list_xattr(fd.raw())?,
                    // A device node: none.
                    Err(ENODATA) => Vec::new(),
                    Err(e) => return Err(e),
                };
                if size == 0 {
                    r.u32(u32::try_from(names.len()).unwrap_or(u32::MAX)).u32(0);
                } else if names.len() > size as usize {
                    return Err(ERANGE);
                } else {
                    r.bytes(&names);
                }
            }
            op::SETXATTR => {
                self.writable(s, nodeid)?;
                let size = a.u32()?;
                let flags = a.u32()?;
                let name = a.cstr()?;
                let value = a.rest().get(..size as usize).ok_or(EINVAL)?;
                if hidden(&name) {
                    return Err(EOPNOTSUPP);
                }
                let host_name = host_xattr(&name)?;
                let fd = node_fd_to_change(s, nodeid)?;
                fset_xattr(fd.raw(), &host_name, value, flags)?;
            }
            op::REMOVEXATTR => {
                self.writable(s, nodeid)?;
                let name = a.cstr()?;
                if hidden(&name) {
                    return Err(ENODATA);
                }
                let host_name = host_xattr(&name).map_err(|_| ENODATA)?;
                let fd = node_fd_to_change(s, nodeid)?;
                fremove_xattr(fd.raw(), &host_name)?;
            }
            op::LSEEK => {
                let fh = a.u64()?;
                let offset = a.u64()?;
                let whence = host::whence(a.u32()?).ok_or(EINVAL)?;
                let at = seek(file_fd(s, fh)?, offset, whence)?;
                r.u64(at);
            }
            op::FALLOCATE => {
                self.writable(s, nodeid)?;
                let fh = a.u64()?;
                let offset = a.u64()?;
                let length = a.u64()?;
                let mode = a.u32()?;
                let fd = file_fd(s, fh)?;
                fallocate(fd, mode, offset, length)?;
            }
            _ => return Err(ENOSYS),
        }
        Ok(())
    }

    /// The node made as `name` in `parent` by `caller`: its owner kept where it is not
    /// root, or where `parent` passes its group on (setgid); then looked up.
    /// Says of node `name`, just made in `parent` by `caller`, what the guest sees of it:
    /// its owner, kept where it is not root's; and what `kept` says of it that the host
    /// file does not, its mode where the host's differs, its kind where the host made a
    /// regular file in its place.
    fn made(
        &self,
        s: &mut State,
        parent: u64,
        name: &CStr,
        caller: (u32, u32),
        kept: Kept,
        r: &mut Reply,
    ) -> Result<u64, Errno> {
        self.record(s, parent, name, caller, kept)?;
        let (id, st, owner) = lookup(s, parent, name)?;
        let dax = self.dax_of(s, id, &st);
        r.entry(id, &st, owner, dax);
        Ok(id)
    }

    /// [`made`](Self::made)'s record of node `name`, unanswered.
    fn record(
        &self,
        s: &mut State,
        parent: u64,
        name: &CStr,
        caller: (u32, u32),
        kept: Kept,
    ) -> Result<(), Errno> {
        let parent_st = node_stat(s, parent)?;
        let parent_owner = node_owner(s, parent, None)?;
        let gid = if mode_of(&parent_st) & S_ISGID != 0 {
            parent_owner.gid
        } else {
            caller.1
        };
        let dir = dir_fd(s, parent)?;
        let mode = match stat_at(dir, name) {
            Ok(host) => kept
                .mode
                .filter(|m| *m != mode_of(&host) & 0o7777 || kept.kind.is_some()),
            Err(_) => kept.mode,
        };
        let owner = Owner {
            uid: caller.0,
            gid,
            mode,
            kind: kept.kind,
            rdev: kept.rdev,
        };
        let recorded = if owner == Owner::default() {
            Ok(())
        } else {
            open_meta(dir, name, None)
                .and_then(|fd| fset_xattr(fd.raw(), &owner_xattr()?, owner.encode().as_bytes(), 0))
        };
        // A node made in another's place is the guest's kind only as recorded: unrecorded,
        // it is no node of the guest's.
        if let (Err(e), Some(_)) = (recorded, kept.kind) {
            let _ = unlink_at(dir, name, false);
            return Err(e);
        }
        Ok(())
    }

    fn setattr(&self, s: &mut State, nodeid: u64, a: &mut Args<'_>) -> Result<(), Errno> {
        let valid = a.u32()?;
        let _ = a.u32()?;
        let fh = a.u64()?;
        let size = a.u64()?;
        let _lock = a.u64()?;
        let atime = a.u64()?;
        let mtime = a.u64()?;
        let _ctime = a.u64()?;
        let atimensec = a.u32()?;
        let mtimensec = a.u32()?;
        let _ = a.u32()?;
        let mut mode = a.u32()?;
        let _ = a.u32()?;
        let uid = a.u32()?;
        let gid = a.u32()?;
        let (dir, name) = at(s, nodeid)?;
        // A directory's own descriptor, which `at` gave: its attributes are changed through it.
        let own = s.nodes.get(&nodeid).is_some_and(|n| n.dir).then_some(dir);
        let mut owner = node_owner(s, nodeid, None)?;
        let had = node_has_owner(s, nodeid);
        if valid & fattr::KILL_SUIDGID != 0 && valid & fattr::MODE == 0 {
            let st = node_stat(s, nodeid)?;
            mode = owner.mode.unwrap_or(mode_of(&st) & 0o7777) & !(S_ISUID | S_ISGID);
            return self.chmod(s, nodeid, own, dir, &name, mode, &mut owner, had);
        }
        if valid & fattr::MODE != 0 {
            self.chmod(s, nodeid, own, dir, &name, mode & 0o7777, &mut owner, had)?;
        }
        if valid & (fattr::UID | fattr::GID) != 0 {
            if valid & fattr::UID != 0 {
                owner.uid = uid;
            }
            if valid & fattr::GID != 0 {
                owner.gid = gid;
            }
            if owner.mode.is_none() {
                owner.mode = Some(mode_of(&node_stat(s, nodeid)?) & 0o7777);
            }
            let fd = node_fd_to_change(s, nodeid)?;
            fset_xattr(fd.raw(), &owner_xattr()?, owner.encode().as_bytes(), 0)?;
        }
        if valid & fattr::SIZE != 0 {
            match valid & fattr::FH != 0 && is_file(s, fh) {
                true => truncate_fd(file_fd(s, fh)?, size)?,
                false => {
                    let fd = open_at(
                        dir,
                        &name,
                        host::O_WRONLY | host::O_NOFOLLOW | host::O_NONBLOCK,
                        0,
                    )?;
                    truncate_fd(fd.raw(), size)?;
                }
            }
        }
        if valid & (fattr::ATIME | fattr::MTIME) != 0 {
            let time = |set: bool, now: bool, s_: u64, ns: u32| libc::timespec {
                tv_sec: s_ as _,
                tv_nsec: if now {
                    libc::UTIME_NOW
                } else if set {
                    host::c_long::from(ns as i32)
                } else {
                    libc::UTIME_OMIT
                },
            };
            let times = [
                time(
                    valid & fattr::ATIME != 0,
                    valid & fattr::ATIME_NOW != 0,
                    atime,
                    atimensec,
                ),
                time(
                    valid & fattr::MTIME != 0,
                    valid & fattr::MTIME_NOW != 0,
                    mtime,
                    mtimensec,
                ),
            ];
            let rc = match own {
                // SAFETY: futimens(2) of a descriptor we hold.
                Some(fd) => unsafe { libc::futimens(fd, times.as_ptr()) },
                // SAFETY: utimensat(2) of a NUL-terminated name in a directory we hold.
                None => unsafe {
                    libc::utimensat(dir, name.as_ptr(), times.as_ptr(), host::AT_SYMLINK_NOFOLLOW)
                },
            };
            if rc != 0 {
                return Err(last());
            }
        }
        Ok(())
    }

    /// renameat2(2)'s RENAME_WHITEOUT, which overlayfs requires of an upper layer it takes
    /// for a remote one (fs/overlayfs/super.c, ovl_make_workdir: "upper fs missing required
    /// features"), and which neither host gives an unprivileged process (macOS has none;
    /// Linux's asks CAP_MKNOD): `old` renamed to `new`, then a whiteout made in its place,
    /// as the guest's kernel makes one (a character device of 0:0, mode 0, the caller's),
    /// kept as any device node the host cannot make is ([`Owner::kind`]). Where the
    /// whiteout cannot be made, the rename is undone, so that the two happen together or
    /// not at all, as on Linux.
    #[allow(clippy::too_many_arguments)]
    fn rename_whiteout(
        &self,
        s: &mut State,
        olddir: u64,
        old: &CStr,
        newdir: u64,
        new: &CStr,
        flags: u32,
        caller: (u32, u32),
    ) -> Result<(), Errno> {
        if flags & linux::RENAME_EXCHANGE != 0 {
            return Err(EINVAL);
        }
        rename(s, olddir, old, newdir, new, flags & !linux::RENAME_WHITEOUT)?;
        let dir = dir_fd(s, olddir)?;
        let whiteout = Kept {
            mode: Some(0),
            kind: Some(S_IFCHR),
            rdev: Some(0),
        };
        let made = open_at(dir, old, host::O_CREAT | host::O_EXCL | host::O_WRONLY, 0o600)
            .map(drop)
            .and_then(|()| self.record(s, olddir, old, caller, whiteout));
        if let Err(e) = made {
            let _ = rename(s, newdir, new, olddir, old, linux::RENAME_NOREPLACE);
            return Err(e);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn chmod(
        &self,
        s: &mut State,
        nodeid: u64,
        own: Option<host::Raw>,
        dir: host::Raw,
        name: &CStr,
        mode: u32,
        owner: &mut Owner,
        had: bool,
    ) -> Result<(), Errno> {
        let host = host_perm(mode, own.is_some());
        // Its open handles a mode refuses keep a descriptor taken before it does (audit V09).
        pin_before_chmod(s, nodeid, dir, name, host);
        match own {
            Some(fd) => chmod_fd(fd, host)?,
            None => chmod_at(dir, name, host)?,
        }
        // The guest's mode is kept where the host's differs, or the attribute is kept.
        if had || host != mode & 0o7777 {
            owner.mode = Some(mode & 0o7777);
            let fd = node_fd_to_change(s, nodeid)?;
            fset_xattr(fd.raw(), &owner_xattr()?, owner.encode().as_bytes(), 0)?;
        }
        Ok(())
    }
}

/// A descriptor of a node, for its attributes: a directory's own, or one opened for it.
enum NodeFd {
    Borrowed(host::Raw),
    Owned(host::Owned),
}

impl NodeFd {
    fn raw(&self) -> host::Raw {
        match self {
            NodeFd::Borrowed(fd) => *fd,
            NodeFd::Owned(fd) => fd.raw(),
        }
    }
}

/// `name` in `dir`, opened only to read and set its attributes: not followed, not waiting
/// on a FIFO's writer, taking no terminal. A device node is not opened, as its driver's
/// open acts (a serial line raises DTR, a tape rewinds as it closes) where the guest only
/// looked, as at an `ls -l` of a shared /dev: it has no attributes here (ENODATA), as
/// Linux gives a special file no `user.` ones (fs/xattr.c `xattr_permission`), the
/// owner's among them (audit V08). `known` is the node's file type where the caller has
/// just looked at it, which spares looking again.
fn open_meta(dir: host::Raw, name: &CStr, known: Option<u32>) -> Result<NodeFd, Errno> {
    let kind = match known {
        Some(kind) => kind,
        None => mode_of(&stat_at(dir, name)?) & S_IFMT,
    };
    if matches!(kind, S_IFCHR | S_IFBLK) {
        return Err(ENODATA);
    }
    #[cfg(target_os = "macos")]
    let flags = host::O_RDONLY | host::O_NOFOLLOW | host::O_NONBLOCK | host::O_NOCTTY | libc::O_SYMLINK;
    #[cfg(target_os = "linux")]
    let flags = host::O_RDONLY | host::O_NOFOLLOW | host::O_NONBLOCK | host::O_NOCTTY;
    open_at(dir, name, flags, 0).map(NodeFd::Owned)
}

fn node_fd(s: &mut State, nodeid: u64) -> Result<NodeFd, Errno> {
    node_fd_known(s, nodeid, None)
}

/// [`node_fd`] of a node whose file type the caller has just looked at, `known`.
fn node_fd_known(s: &mut State, nodeid: u64, known: Option<u32>) -> Result<NodeFd, Errno> {
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    if node.dir {
        return Ok(NodeFd::Borrowed(dir_fd(s, nodeid)?));
    }
    let (dir, name) = reach(s, nodeid)?;
    open_meta(dir, &name, known)
}

/// A node's descriptor to change its attributes: a device node, which has none here, is
/// refused as Linux refuses a special file `user.` ones (EPERM).
fn node_fd_to_change(s: &mut State, nodeid: u64) -> Result<NodeFd, Errno> {
    node_fd(s, nodeid).map_err(|e| if e == ENODATA { EPERM } else { e })
}

fn node_has_owner(s: &mut State, nodeid: u64) -> bool {
    node_fd(s, nodeid)
        .ok()
        .and_then(|fd| fget_xattr(fd.raw(), &owner_xattr().ok()?).ok().flatten())
        .is_some()
}

/// The guest's owner of a node: its attribute's, or root. `st`, the node's own stat where
/// the caller has just taken it.
fn node_owner(s: &mut State, nodeid: u64, st: Option<&Stat>) -> Result<Owner, Errno> {
    let Ok(fd) = node_fd_known(s, nodeid, st.map(|st| mode_of(st) & S_IFMT)) else {
        return Ok(Owner::default());
    };
    Ok(fget_xattr(fd.raw(), &owner_xattr()?)
        .ok()
        .flatten()
        .and_then(|v| Owner::parse(&v))
        .unwrap_or_default())
}

/// Directory node `nodeid`'s descriptor: the root's; one held; or one opened again along
/// the node's path from the nearest of its ancestors held, or the root, and checked to be
/// the directory the node was. One renamed or replaced on the host since is stale
/// (ESTALE), which has the guest's kernel look it up again (audit V09).
fn dir_fd(s: &mut State, nodeid: u64) -> Result<host::Raw, Errno> {
    if nodeid == ROOT {
        return Ok(s.root.raw());
    }
    if let Some(v) = s.volumes.get(&nodeid) {
        return Ok(v.fd.raw());
    }
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    if !node.dir {
        return Err(ENOTDIR);
    }
    let want = node.key;
    if let Some(Held::Fd(fd)) = s.fds.get(Slot::Node(nodeid)) {
        return Ok(fd.raw());
    }
    // The names down to it; a chain that breaks (a node the guest forgot) or loops (a
    // directory found again under one of its own, through a bind mount) leads nowhere.
    let mut names = Vec::new();
    let mut at = nodeid;
    let from = loop {
        let node = s.nodes.get(&at).ok_or(ESTALE)?;
        names.push(node.name.clone());
        at = node.parent;
        if at == ROOT {
            break s.root.raw();
        }
        if let Some(v) = s.volumes.get(&at) {
            break v.fd.raw();
        }
        if let Some(Held::Fd(fd)) = s.fds.get(Slot::Node(at)) {
            break fd.raw();
        }
        if names.len() > s.nodes.len() {
            return Err(ESTALE);
        }
    };
    names.reverse();
    let fd = open_beneath(from, &names)?;
    let st = stat_fd(fd.raw())?;
    if !is_dir(&st) || key(&st) != want {
        return Err(ESTALE);
    }
    let raw = fd.raw();
    s.fds.insert(Slot::Node(nodeid), Held::Fd(fd));
    Ok(raw)
}

/// The directory `names` lead to under `dir`, a component after another, none followed
/// if a symlink: on Linux in one openat2(2) refusing symlinks and any way out of `dir`
/// (RESOLVE_NO_SYMLINKS, RESOLVE_BENEATH; Linux 5.6), and a component at a time with
/// O_NOFOLLOW where the kernel has no openat2, or the path is too long for one call, and on
/// macOS. A path that no longer leads to a directory is stale.
fn open_beneath(dir: host::Raw, names: &[CString]) -> Result<host::Owned, Errno> {
    #[cfg(target_os = "linux")]
    if let Some(opened) = openat2_beneath(dir, names) {
        return opened.map_err(stale);
    }
    let mut held: Option<host::Owned> = None;
    for name in names {
        let at = held.as_ref().map_or(dir, std::os::fd::AsRawFd::as_raw_fd);
        let next = open_at(at, name, host::O_RDONLY | host::O_DIRECTORY | host::O_NOFOLLOW, 0);
        held = Some(next.map_err(stale)?);
    }
    held.ok_or(EINVAL)
}

/// [`open_beneath`] in one openat2(2); none where this kernel has no openat2 (ENOSYS, or a
/// seccomp filter's EPERM) or the path is longer than one call takes.
#[cfg(target_os = "linux")]
fn openat2_beneath(dir: host::Raw, names: &[CString]) -> Option<Result<host::Owned, Errno>> {
    /// struct open_how (include/uapi/linux/openat2.h), which libc's leaves unbuilt.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let mut path = Vec::new();
    for name in names {
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name.to_bytes());
    }
    let path = CString::new(path).ok()?;
    let how = OpenHow {
        flags: u64::try_from(host::O_RDONLY | host::O_DIRECTORY | host::O_CLOEXEC).ok()?,
        mode: 0,
        resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: openat2(2) of a NUL-terminated path, with an open_how of the size given.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir,
            path.as_ptr(),
            &raw const how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if let Ok(fd) = host::Raw::try_from(fd)
        && fd >= 0
    {
        // SAFETY: a fresh descriptor nothing else owns.
        return Some(Ok(unsafe { host::Owned::from_raw_fd(fd) }));
    }
    match last() {
        ENOSYS | EPERM | ENAMETOOLONG => None,
        e => Some(Err(e)),
    }
}

/// An error opening a node's path, as the guest hears it: a path that no longer leads to a
/// directory within the share (gone, not one, a symlink, a way out) is stale.
fn stale(e: Errno) -> Errno {
    match e {
        ENOENT | ENOTDIR | ELOOP | EXDEV => ESTALE,
        e => e,
    }
}

/// A node's parent's descriptor and its name; a directory's own and `.`.
fn at(s: &mut State, nodeid: u64) -> Result<(host::Raw, CString), Errno> {
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    if node.dir {
        return Ok((dir_fd(s, nodeid)?, cstring(".")?));
    }
    reach(s, nodeid)
}

/// File node `nodeid`'s directory and name: the name the guest last looked it up by; or,
/// where that name no longer leads to it (unlinked or renamed away, as overlayfs unlinks
/// the links of its shared whiteout), another of the names the guest knows it by that
/// does, which becomes its own. Only a file the guest knows by several names is looked at
/// here; a name that leads to no file of its number is left for the caller's call to say
/// so (ENOENT).
fn reach(s: &mut State, nodeid: u64) -> Result<(host::Raw, CString), Errno> {
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    let (parent, name, own) = (node.parent, node.name.clone(), node.key);
    let links = node.links.clone();
    let dir = dir_fd(s, parent)?;
    if links.is_empty() || stat_at(dir, &name).is_ok_and(|st| key(&st) == own) {
        return Ok((dir, name));
    }
    for (i, (p, n)) in links.iter().enumerate() {
        let Ok(d) = dir_fd(s, *p) else {
            continue;
        };
        if stat_at(d, n).is_ok_and(|st| key(&st) == own) {
            if let Some(node) = s.nodes.get_mut(&nodeid) {
                node.links.remove(i);
                (node.parent, node.name) = (*p, n.clone());
            }
            return Ok((d, n.clone()));
        }
    }
    Ok((dir, name))
}

fn node_stat(s: &mut State, nodeid: u64) -> Result<Stat, Errno> {
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    if node.dir {
        return stat_fd(dir_fd(s, nodeid)?);
    }
    let (dir, name) = reach(s, nodeid)?;
    stat_at(dir, &name)
}

/// `name` in directory node `parent`: its node, counted once more and known by this name
/// now, its attributes and its owner. A directory is not opened until it is used.
fn lookup(s: &mut State, parent: u64, name: &CStr) -> Result<(u64, Stat, Owner), Errno> {
    // A join share's root: its volumes, by name (D119).
    if s.joined && parent == ROOT {
        let id = *s.named.get(name).ok_or(ENOENT)?;
        let st = stat_fd(dir_fd(s, id)?)?;
        if let Some(n) = s.nodes.get_mut(&id) {
            n.lookups = n.lookups.saturating_add(1);
        }
        let owner = node_owner(s, id, Some(&st))?;
        return Ok((id, st, owner));
    }
    let volume = volume_of(s, parent);
    let dir = dir_fd(s, parent)?;
    let st = stat_at(dir, name)?;
    let k = key(&st);
    let at = (volume, k.0, k.1);
    // The root, and a volume's, keep their names.
    let rooted = |s: &State, id: u64| id == ROOT || s.volumes.contains_key(&id);
    let id = match s.by_key.get(&at).copied().filter(|id| s.nodes.contains_key(id)) {
        Some(id) => {
            let fixed = rooted(s, id);
            if let Some(n) = s.nodes.get_mut(&id) {
                n.lookups = n.lookups.saturating_add(1);
                if !fixed {
                    let was = (n.parent, std::mem::replace(&mut n.name, name.to_owned()));
                    (n.parent, n.dir) = (parent, is_dir(&st));
                    // The name it was known by is one of its links now, a file's.
                    if n.dir {
                        n.links.clear();
                    } else if (was.0, was.1.as_c_str()) != (parent, name) && !n.links.contains(&was) {
                        n.links.push(was);
                    }
                    n.links.retain(|(p, l)| (*p, l.as_c_str()) != (parent, name));
                }
                // Its inode a directory's no longer, deleted and its number taken again: the
                // descriptor held is that directory's.
                if !n.dir {
                    s.fds.remove(Slot::Node(id));
                }
            }
            id
        }
        None => {
            let id = fresh_node(s);
            s.nodes.insert(
                id,
                Node {
                    dir: is_dir(&st),
                    parent,
                    name: name.to_owned(),
                    links: Vec::new(),
                    lookups: 1,
                    key: k,
                    volume,
                },
            );
            s.by_key.insert(at, id);
            id
        }
    };
    let owner = node_owner(s, id, Some(&st))?;
    Ok((id, st, owner))
}

/// A node ID no node has, nor the root's or [`NO_VOLUME`]: the next of the share's keyed
/// hashes of a count.
fn fresh_node(s: &mut State) -> u64 {
    use std::hash::BuildHasher as _;
    loop {
        s.next_node = s.next_node.wrapping_add(1);
        let id = s.ids.hash_one(s.next_node);
        if id != NO_VOLUME && id != ROOT && !s.nodes.contains_key(&id) {
            return id;
        }
    }
}

/// Forgets all the guest's kernel knew of the share as its session ends: it sends DESTROY
/// as the share's last mount goes, and no FORGET for the nodes it held then, as it forgets a
/// node only while the share is mounted (fs/fuse/inode.c, fuse_evict_inode, on SB_ACTIVE).
/// The root's node stays, as the next session's root, and a join share's volumes', none
/// looked up; every other node, handle and descriptor goes. A join share's lives as long as
/// its microVM, mounted again by each joiner that brings volumes after the last has ended.
fn reset(s: &mut State) {
    let volumes = &s.volumes;
    s.nodes.retain(|id, n| {
        n.lookups = u64::from(*id == ROOT);
        *id == ROOT || volumes.contains_key(id)
    });
    let nodes = &s.nodes;
    s.by_key.retain(|_, id| nodes.contains_key(id));
    s.handles.clear();
    s.opened.clear();
    s.fds = Fds::default();
    s.pins.clear();
    s.unreached.clear();
}

fn forget(s: &mut State, nodeid: u64, n: u64) {
    if nodeid == ROOT {
        return;
    }
    // A volume's root, while it is served, is its node for as long (D119).
    let served = s.volumes.contains_key(&nodeid);
    let gone = match s.nodes.get_mut(&nodeid) {
        Some(node) => {
            node.lookups = node.lookups.saturating_sub(n);
            node.lookups == 0 && !served
        }
        None => false,
    };
    if gone && let Some(node) = s.nodes.remove(&nodeid) {
        s.fds.remove(Slot::Node(nodeid));
        let at = (node.volume, node.key.0, node.key.1);
        if s.by_key.get(&at) == Some(&nodeid) {
            s.by_key.remove(&at);
        }
    }
}

/// What a share of one name, `only`, the one name of directory node `top`, lets the guest
/// do: reach that name in it and nothing else there, and rename or link nothing.
fn only_confined(only: &CStr, top: u64, opcode: u32, nodeid: u64, body: &[u8]) -> Result<(), Errno> {
    match opcode {
        op::RENAME | op::RENAME2 | op::LINK => Err(EBUSY),
        _ if nodeid != top => Ok(()),
        op::LOOKUP if Args(body).name()?.as_c_str() == only => Ok(()),
        op::LOOKUP => Err(ENOENT),
        op::UNLINK if Args(body).name()?.as_c_str() == only => Err(EBUSY),
        _ if reads(opcode) => Ok(()),
        _ => Err(EACCES),
    }
}

/// What a join share lets the guest do (D119): in its root, find its volumes by name and
/// read, nothing made; in a volume of one name, what a share of one name lets it.
fn joined_confined(s: &State, opcode: u32, nodeid: u64, body: &[u8]) -> Result<(), Errno> {
    if nodeid == ROOT {
        return if opcode == op::LOOKUP || reads(opcode) {
            Ok(())
        } else {
            Err(EACCES)
        };
    }
    let volume = s.nodes.get(&nodeid).map_or(NO_VOLUME, |n| n.volume);
    match s.volumes.get(&volume).and_then(|v| v.only.as_deref()) {
        Some(only) => only_confined(only, volume, opcode, nodeid, body),
        None => Ok(()),
    }
}

/// Whether `opcode` only reads, or opens and closes a directory to read it.
fn reads(opcode: u32) -> bool {
    matches!(
        opcode,
        op::INIT
            | op::DESTROY
            | op::GETATTR
            | op::STATFS
            | op::OPENDIR
            | op::READDIR
            | op::READDIRPLUS
            | op::RELEASEDIR
            | op::FSYNCDIR
            | op::ACCESS
            | op::GETXATTR
            | op::LISTXATTR
            | op::SYNCFS
    )
}

/// The volume node `nodeid` is in; [`NO_VOLUME`] for none.
fn volume_of(s: &State, nodeid: u64) -> u64 {
    s.nodes.get(&nodeid).map_or(NO_VOLUME, |n| n.volume)
}

/// The handles a server keeps at most: Linux's own bound on a process's open files,
/// `fs.nr_open`'s default (fs/file.c `sysctl_nr_open`, 1024 * 1024), which a guest's
/// kernel keeps each of its processes to. A handle takes no descriptor (`State::fds`), but
/// a hostile guest's would cost the host memory without bound.
const MAX_HANDLES: usize = 1 << 20;

/// Room for another of the guest's handles: past `MAX_HANDLES`, EMFILE, as a kernel
/// refuses a process past its limit.
fn room_for_handle(s: &State) -> Result<(), Errno> {
    if s.handles.len() >= MAX_HANDLES {
        return Err(EMFILE);
    }
    Ok(())
}

/// Host open `flags` without those that act once, for a handle's opens after its first.
fn once(flags: host::c_int) -> host::c_int {
    flags & !(host::O_CREAT | host::O_EXCL | host::O_TRUNC)
}

/// Whether a descriptor opened with access mode `access` serves `flags`' access.
fn allows(access: host::c_int, flags: host::c_int) -> bool {
    match flags & host::O_ACCMODE {
        host::O_RDONLY => access != host::O_WRONLY,
        host::O_WRONLY => access != host::O_RDONLY,
        _ => access == host::O_RDWR,
    }
}

/// A handle of the guest's on `node`, opened with host `flags` (a file's) or none (a
/// directory's), its descriptor `held` where the open took one.
fn add_handle(s: &mut State, node: u64, flags: Option<host::c_int>, held: Option<Held>) -> u64 {
    let fh = s.next_handle;
    s.next_handle += 1;
    let opened = s.opened.entry(node).or_default();
    opened.handles += 1;
    if let Some(flags) = flags {
        let access = flags & host::O_ACCMODE;
        if access != host::O_WRONLY {
            opened.reading += 1;
        }
        if access != host::O_RDONLY {
            opened.writing += 1;
        }
    }
    s.handles.insert(fh, Handle { node, flags });
    if let Some(held) = held {
        s.fds.insert(Slot::Handle(fh), held);
    }
    fh
}

/// Lets go of handle `fh`, and of its node's pin with the node's last handle.
fn release(s: &mut State, fh: u64) {
    let Some(h) = s.handles.remove(&fh) else {
        return;
    };
    s.fds.remove(Slot::Handle(fh));
    let Some(opened) = s.opened.get_mut(&h.node) else {
        return;
    };
    opened.handles = opened.handles.saturating_sub(1);
    if let Some(flags) = h.flags {
        let access = flags & host::O_ACCMODE;
        if access != host::O_WRONLY {
            opened.reading = opened.reading.saturating_sub(1);
        }
        if access != host::O_RDONLY {
            opened.writing = opened.writing.saturating_sub(1);
        }
    }
    if opened.handles == 0 {
        s.opened.remove(&h.node);
        s.pins.remove(&h.node);
        s.unreached.remove(&h.node);
    }
}

/// Whether `fh` is a file handle of the guest's.
fn is_file(s: &State, fh: u64) -> bool {
    s.handles.get(&fh).is_some_and(|h| h.flags.is_some())
}

/// File handle `fh`'s descriptor: its node's pin, where the guest unlinked or renamed over
/// it; one held; or one opened again along its node's path with the handle's flags, and
/// checked to be the file it was. A handle whose file no path reaches, unpinned, or whose
/// path leads to another file now (renamed or replaced on the host), is stale (ESTALE),
/// which the guest's kernel reports as it reports a stale NFS handle (audit V09).
fn file_fd(s: &mut State, fh: u64) -> Result<host::Raw, Errno> {
    let h = s.handles.get(&fh).ok_or(EBADF)?;
    let (node, flags) = (h.node, h.flags.ok_or(EBADF)?);
    if let Some((pin, _)) = s.pins.get(&node) {
        return Ok(pin.raw());
    }
    if let Some(Held::Fd(fd)) = s.fds.get(Slot::Handle(fh)) {
        return Ok(fd.raw());
    }
    if s.unreached.get(&node).is_some_and(|&first| fh < first) {
        return Err(ESTALE);
    }
    let want = s.nodes.get(&node).ok_or(ESTALE)?.key;
    let (dir, name) = at(s, node)?;
    // Refused for want of access, it is stale too: the host took its mode away.
    let (fd, st) = open_regular(dir, &name, flags, 0).map_err(|e| match e {
        EBADF | EACCES | EPERM => ESTALE,
        e => stale(e),
    })?;
    if key(&st) != want {
        return Err(ESTALE);
    }
    let raw = fd.raw();
    s.fds.insert(Slot::Handle(fh), Held::Fd(fd));
    Ok(raw)
}

/// Directory handle `fh`'s stream: one held, or one opened again on its node, or on its
/// pin where the guest removed it; on macOS a stream opened again starts at the
/// directory's start, and a READDIR at an offset past it reads up to it.
fn dir_stream(s: &mut State, fh: u64) -> Result<&mut DirStream, Errno> {
    let h = s.handles.get(&fh).ok_or(EBADF)?;
    if h.flags.is_some() {
        return Err(EBADF);
    }
    let node = h.node;
    if !matches!(s.fds.get(Slot::Handle(fh)), Some(Held::Dir(_))) {
        let dir = match s.pins.get(&node) {
            Some((pin, _)) => pin.raw(),
            None if s.unreached.get(&node).is_some_and(|&first| fh < first) => return Err(ESTALE),
            None => dir_fd(s, node)?,
        };
        let stream = DirStream::open(dir)?;
        s.fds.insert(Slot::Handle(fh), Held::Dir(stream));
    }
    match s.fds.get(Slot::Handle(fh)) {
        Some(Held::Dir(stream)) => Ok(stream),
        _ => Err(EBADF),
    }
}

/// Before the guest removes `name` from directory `dir` (unlinks it, or renames another
/// over it): a node there it has open keeps one descriptor, pinned, with the access its
/// handles have, so that they go on serving once no path reaches it, as a kernel's open
/// files do after an unlink. Pins are the descriptors the server cannot let go, and count
/// against its budget: past it, none is taken, and the node's handles answer ESTALE once it
/// is removed (audit V09).
fn pin_before_removal(s: &mut State, volume: u64, dir: host::Raw, name: &CStr) -> Removal {
    let Ok(st) = stat_at(dir, name) else {
        return Removal::Nothing;
    };
    let (dev, ino) = key(&st);
    let Some(&node) = s.by_key.get(&(volume, dev, ino)) else {
        return Removal::Nothing;
    };
    let Some(opened) = s.opened.get(&node) else {
        return Removal::Nothing;
    };
    if s.pins.contains_key(&node) {
        return Removal::Nothing;
    }
    let access = match (opened.reading > 0, opened.writing > 0) {
        (_, false) => host::O_RDONLY,
        (false, true) => host::O_WRONLY,
        (true, true) => host::O_RDWR,
    };
    // A file the removal leaves another name of needs no pin where there is no room: that
    // name reaches it, and the guest's lookup of it makes it the node's path.
    if !room_for_pin(s) && !is_dir(&st) && st.nlink > 1 {
        return Removal::Nothing;
    }
    let fd = if !room_for_pin(s) {
        None
    } else if is_dir(&st) {
        open_at(
            dir,
            name,
            host::O_RDONLY | host::O_DIRECTORY | host::O_NOFOLLOW,
            0,
        )
        .ok()
    } else {
        open_regular(dir, name, access | host::O_NOFOLLOW, 0)
            .ok()
            .map(|(fd, _)| fd)
    };
    match fd.filter(|fd| stat_fd(fd.raw()).is_ok_and(|now| key(&now) == key(&st))) {
        Some(fd) => {
            s.pins.insert(node, (fd, access));
            Removal::Pinned(node)
        }
        None => {
            // Its handles opened before now are stale once their own descriptors go;
            // one opened after reaches what its path leads to then.
            s.unreached.insert(node, s.next_handle);
            Removal::Unreached(node)
        }
    }
}

/// Before the guest changes `name`'s mode in `dir` (node `node`) to `mode`: where the mode
/// refuses the access its open handles have, one descriptor taken while the old mode allows
/// it, pinned, so that they go on serving, as a kernel's open files do after a chmod. Past
/// the budget none is taken, and they are stale once their own descriptors go.
fn pin_before_chmod(s: &mut State, node: u64, dir: host::Raw, name: &CStr, mode: u32) {
    if s.pins.contains_key(&node) || !room_for_pin(s) {
        return;
    }
    let Some(opened) = s.opened.get(&node) else {
        return;
    };
    let access = match (opened.reading > 0, opened.writing > 0) {
        (_, false) => host::O_RDONLY,
        (false, true) => host::O_WRONLY,
        (true, true) => host::O_RDWR,
    };
    if !refuses(mode, access) {
        return;
    }
    let Some(want) = s.nodes.get(&node).map(|n| n.key) else {
        return;
    };
    let fd = match stat_at(dir, name) {
        Ok(st) if is_dir(&st) => open_at(
            dir,
            name,
            host::O_RDONLY | host::O_DIRECTORY | host::O_NOFOLLOW,
            0,
        )
        .ok(),
        Ok(_) => open_regular(dir, name, access | host::O_NOFOLLOW, 0)
            .ok()
            .map(|(fd, _)| fd),
        Err(_) => None,
    };
    if let Some(fd) = fd.filter(|fd| stat_fd(fd.raw()).is_ok_and(|now| key(&now) == want)) {
        s.pins.insert(node, (fd, access));
    }
}

/// What [`pin_before_removal`] did, for [`removed`] to undo should the removal fail.
#[derive(Debug, Clone, Copy)]
enum Removal {
    Nothing,
    Pinned(u64),
    Unreached(u64),
}

/// After a removal [`pin_before_removal`] made ready for: where it failed, the name still
/// reaches the node, so its pin is let go, or it is reached again.
fn removed(s: &mut State, removal: Removal, done: bool) {
    if done {
        return;
    }
    match removal {
        Removal::Nothing => {}
        Removal::Pinned(node) => {
            s.pins.remove(&node);
        }
        Removal::Unreached(node) => {
            s.unreached.remove(&node);
        }
    }
}

/// Whether the budget has room for one more pin: pins alone count against it, as the
/// descriptors the server cannot let go.
fn room_for_pin(s: &State) -> bool {
    u64::try_from(s.pins.len()).unwrap_or(u64::MAX) < s.budget
}

/// Whether a file of `mode`, its owner's, refuses an open with `flags`' access to its owner,
/// the share process: once its handle's descriptor were let go, none could be opened for it
/// again by its path.
fn refuses(mode: u32, flags: host::c_int) -> bool {
    let access = flags & host::O_ACCMODE;
    let reads = access != host::O_WRONLY;
    let writes = access != host::O_RDONLY;
    (reads && mode & 0o400 == 0) || (writes && mode & 0o200 == 0)
}

/// A directory's entry: its name, inode number and Linux dirent type.
#[derive(Debug, Clone)]
struct Entry {
    name: CString,
    ino: u64,
    kind: u32,
}

/// A directory the guest opened, read as it asks from a descriptor of its own. A handle
/// holds none of the listing: a copy of it each, taken at OPENDIR, made a guest that opened
/// a large directory again and again, never releasing it, cost the host memory without
/// bound (audit V05). Handles are as many as the descriptors the process may have.
///
/// Linux: read by getdents64(2) at the offset each request carries, the kernel's cookie
/// for what follows an entry (`d_off`), as virtiofsd reads one; nothing but the
/// descriptor is kept between requests (glibc's own stream keeps 32 KiB, PM M130).
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct DirStream(host::Owned);

#[cfg(target_os = "linux")]
impl DirStream {
    fn open(dir: host::Raw) -> Result<DirStream, Errno> {
        open_at(dir, c".", host::O_RDONLY | host::O_DIRECTORY, 0).map(DirStream)
    }

    /// Gives `each` the entries from `offset` on, with the offset that follows each, until
    /// it says to stop or the directory ends; `want` bytes of them are read at a time.
    fn read(
        &mut self,
        offset: u64,
        want: usize,
        mut each: impl FnMut(&Entry, u64) -> bool,
    ) -> Result<(), Errno> {
        let fd = self.0.raw();
        // SAFETY: lseek(2) of a descriptor we hold.
        if unsafe { libc::lseek(fd, offset as host::off_t, host::SEEK_SET) } < 0 {
            return Err(last());
        }
        // Room for a record of the longest name, 280 bytes, whatever the guest wants.
        let mut buf = vec![0u8; want.clamp(1024, MAX_WRITE as usize)];
        loop {
            // SAFETY: getdents64(2) into a buffer of its length.
            let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
            let got = usize::try_from(n).map_err(|_| last())?;
            if got == 0 {
                return Ok(());
            }
            let mut records = buf.get(..got).unwrap_or_default();
            // struct linux_dirent64: d_ino, d_off, d_reclen, d_type, then the name.
            while !records.is_empty() {
                let u64_at = |at: usize| {
                    let bytes = records.get(at..at + 8)?;
                    Some(u64::from_ne_bytes(bytes.try_into().ok()?))
                };
                let (Some(ino), Some(next)) = (u64_at(0), u64_at(8)) else {
                    return Err(EIO);
                };
                let reclen = records
                    .get(16..18)
                    .and_then(|b| b.try_into().ok())
                    .map(u16::from_ne_bytes)
                    .ok_or(EIO)?;
                let record = records
                    .get(..usize::from(reclen))
                    .filter(|r| r.len() > 19)
                    .ok_or(EIO)?;
                let name = record.get(19..).unwrap_or_default();
                let entry = Entry {
                    name: CStr::from_bytes_until_nul(name).map_err(|_| EIO)?.to_owned(),
                    ino,
                    kind: u32::from(record.get(18).copied().unwrap_or(0)),
                };
                if !each(&entry, next) {
                    return Ok(());
                }
                records = records.get(usize::from(reclen)..).unwrap_or_default();
            }
        }
    }
}

/// macOS: a directory stream of the handle's own (2.2 KiB, PM M130), its position counted
/// in entries, the offsets requests carry, with the entry the last reply had no room for;
/// a request at any other offset reads again from the start.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct DirStream {
    stream: std::ptr::NonNull<libc::DIR>,
    /// Entries read since the stream's start.
    at: u64,
    /// The entry at `at`, read already, which the last reply had no room for.
    held: Option<Entry>,
}

// SAFETY: the stream is this value's alone, used by one thread at a time (under the
// server's state lock); a DIR holds nothing tied to the thread that opened it.
#[cfg(target_os = "macos")]
unsafe impl Send for DirStream {}

#[cfg(target_os = "macos")]
impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: closedir(3) of the stream this value owns, which closes its descriptor.
        unsafe { libc::closedir(self.stream.as_ptr()) };
    }
}

#[cfg(target_os = "macos")]
impl DirStream {
    fn open(dir: host::Raw) -> Result<DirStream, Errno> {
        use std::os::fd::IntoRawFd as _;
        let fd = open_at(dir, c".", host::O_RDONLY | host::O_DIRECTORY, 0)?;
        // SAFETY: fdopendir(3) of a descriptor we own; the stream owns it once it exists.
        let stream = std::ptr::NonNull::new(unsafe { libc::fdopendir(fd.raw()) }).ok_or_else(last)?;
        let _ = fd.into_raw_fd();
        Ok(DirStream {
            stream,
            at: 0,
            held: None,
        })
    }

    fn next(&mut self) -> Option<Entry> {
        // SAFETY: readdir(3) of our open stream; the entry is copied before the next call.
        let entry = unsafe { libc::readdir(self.stream.as_ptr()).as_ref() }?;
        Some(Entry {
            // SAFETY: readdir's entry, its name NUL-terminated.
            name: unsafe { CStr::from_ptr(entry.d_name.as_ptr()) }.to_owned(),
            ino: entry.d_ino,
            kind: u32::from(entry.d_type),
        })
    }

    /// Gives `each` the entries from `offset` on, with the offset that follows each, until
    /// it says to stop or the directory ends.
    fn read(
        &mut self,
        offset: u64,
        _want: usize,
        mut each: impl FnMut(&Entry, u64) -> bool,
    ) -> Result<(), Errno> {
        if offset != self.at {
            // SAFETY: rewinddir(3) of our open stream.
            unsafe { libc::rewinddir(self.stream.as_ptr()) };
            self.at = 0;
            self.held = None;
            while self.at < offset {
                if self.next().is_none() {
                    return Ok(());
                }
                self.at += 1;
            }
        }
        loop {
            let Some(entry) = self.held.take().or_else(|| self.next()) else {
                return Ok(());
            };
            if !each(&entry, self.at.saturating_add(1)) {
                self.held = Some(entry);
                return Ok(());
            }
            self.at += 1;
        }
    }
}

fn rename(s: &mut State, olddir: u64, old: &CStr, newdir: u64, new: &CStr, flags: u32) -> Result<(), Errno> {
    let (od, nd) = (dir_fd(s, olddir)?, dir_fd(s, newdir)?);
    // What the rename replaces, unless it is the file renamed (a link of it) or an
    // exchange, which removes nothing.
    let same = matches!((stat_at(od, old), stat_at(nd, new)), (Ok(a), Ok(b)) if key(&a) == key(&b));
    let removal = if flags & linux::RENAME_EXCHANGE == 0 && !same {
        pin_before_removal(s, volume_of(s, newdir), nd, new)
    } else {
        Removal::Nothing
    };
    let rc = renamed(od, old, nd, new, flags);
    removed(s, removal, rc.is_ok());
    rc?;
    // The names nodes are known by follow, directories' as files': a directory let go is
    // found again by its path.
    let exchange = flags & linux::RENAME_EXCHANGE != 0;
    for node in s.nodes.values_mut() {
        if node.parent == olddir && node.name.as_c_str() == old {
            (node.parent, node.name) = (newdir, new.to_owned());
        } else if exchange && node.parent == newdir && node.name.as_c_str() == new {
            (node.parent, node.name) = (olddir, old.to_owned());
        }
        for link in &mut node.links {
            if link.0 == olddir && link.1.as_c_str() == old {
                *link = (newdir, new.to_owned());
            } else if exchange && link.0 == newdir && link.1.as_c_str() == new {
                *link = (olddir, old.to_owned());
            }
        }
    }
    Ok(())
}

/// `renameat2(2)` of `old` in `od` to `new` in `nd` with Linux's `flags`, as the host makes
/// it: `renameat`, macOS's `renameatx_np`, or Linux's own system call.
fn renamed(od: host::Raw, old: &CStr, nd: host::Raw, new: &CStr, flags: u32) -> Result<(), Errno> {
    let rc = if flags == 0 {
        // SAFETY: renameat(2) of NUL-terminated names in directories we hold.
        unsafe { libc::renameat(od, old.as_ptr(), nd, new.as_ptr()) }
    } else {
        #[cfg(target_os = "macos")]
        {
            let mut host = 0;
            if flags & linux::RENAME_NOREPLACE != 0 {
                host |= libc::RENAME_EXCL;
            }
            if flags & linux::RENAME_EXCHANGE != 0 {
                host |= libc::RENAME_SWAP;
            }
            if flags & !(linux::RENAME_NOREPLACE | linux::RENAME_EXCHANGE) != 0 {
                return Err(EINVAL);
            }
            // SAFETY: renameatx_np(2) of NUL-terminated names in directories we hold.
            unsafe { libc::renameatx_np(od, old.as_ptr(), nd, new.as_ptr(), host) }
        }
        #[cfg(target_os = "linux")]
        {
            // The system call itself: musl 1.2.5, which Rust's musl targets link, has no
            // renameat2 wrapper, and glibc's is the same call.
            // SAFETY: renameat2(2) of NUL-terminated names in directories we hold.
            unsafe {
                libc::syscall(libc::SYS_renameat2, od, old.as_ptr(), nd, new.as_ptr(), flags) as host::c_int
            }
        }
    };
    if rc != 0 {
        return Err(last());
    }
    Ok(())
}

fn hidden(name: &CStr) -> bool {
    let n = name.to_bytes();
    n == OWNER_XATTR.as_bytes() || n.strip_prefix(b"user.") == Some(OWNER_XATTR.as_bytes())
}

/// A listing's `d_type` of a node of kind `kind` (`S_IFCHR` and the rest).
fn dirent_kind(kind: u32) -> u32 {
    u32::from(match kind {
        S_IFCHR => host::DT_CHR,
        S_IFBLK => host::DT_BLK,
        S_IFIFO => host::DT_FIFO,
        S_IFSOCK => host::DT_SOCK,
        _ => host::DT_REG,
    })
}

/// The kind of node `name` in `dir` is to the guest where the host keeps an empty
/// regular file in its place ([`Owner::kind`]): none for any other. Only an empty file
/// the share made so (`0600`) is asked of its attribute, so that a listing reads no
/// attribute of the rest.
fn emulated_at(dir: host::Raw, name: &CStr) -> Option<u32> {
    let st = stat_at(dir, name).ok()?;
    if mode_of(&st) & S_IFMT != S_IFREG || st.size != 0 || mode_of(&st) & 0o7777 != 0o600 {
        return None;
    }
    let fd = open_meta(dir, name, Some(S_IFREG)).ok()?;
    let owner = fget_xattr(fd.raw(), &owner_xattr().ok()?).ok().flatten()?;
    Owner::parse(&owner)?.kind
}

/// Where the host keeps a guest's extended attribute `name`: under its own name on macOS,
/// which keeps no namespaces; and on Linux, where only a privileged process sets one in
/// `trusted.` or `security.` (xattr(7)), those under `user.shards.` (overlayfs keeps its
/// `trusted.overlay.` ones on its upper directory). The guest's names in `user.` are its
/// own; `user.shards.` is the server's, as the owner's attribute is.
fn host_xattr(name: &CStr) -> Result<CString, Errno> {
    let n = name.to_bytes();
    if n.starts_with(PRIVATE_XATTR) {
        return Err(EOPNOTSUPP);
    }
    #[cfg(target_os = "linux")]
    if n.starts_with(b"trusted.") || n.starts_with(b"security.") {
        let mut host = PRIVATE_XATTR.to_vec();
        host.extend_from_slice(n);
        return CString::new(host).map_err(|_| EINVAL);
    }
    Ok(name.to_owned())
}

/// The prefix the host keeps the guest's `trusted.` and `security.` attributes under on
/// Linux ([`host_xattr`]), which a guest's own names may not take.
const PRIVATE_XATTR: &[u8] = b"user.shards.";

/// The guest's name for a host attribute `host`: [`host_xattr`]'s, turned back; none for
/// one the guest does not see: the server's own, another of `user.shards.`'s, and on Linux
/// the host's own `trusted.` and `security.` attributes, which its kernel keeps (an SELinux
/// label, file capabilities or an IMA hash the host set), where the guest's of those names
/// are under `user.shards.`, and reading them by those names reaches the guest's alone.
fn guest_xattr(host: &[u8]) -> Option<&[u8]> {
    match host.strip_prefix(PRIVATE_XATTR) {
        #[cfg(target_os = "linux")]
        Some(rest) if rest.starts_with(b"trusted.") || rest.starts_with(b"security.") => Some(rest),
        Some(_) => None,
        #[cfg(target_os = "linux")]
        None if host.starts_with(b"trusted.") || host.starts_with(b"security.") => None,
        None => Some(host),
    }
}

/// The names of `fd`'s attributes the guest may see: those in a Linux namespace, but for
/// the server's own.
fn list_xattr(fd: host::Raw) -> Result<Vec<u8>, Errno> {
    let mut buf = vec![0u8; 4096];
    let n = loop {
        // SAFETY: flistxattr(2) into a buffer of its length.
        #[cfg(target_os = "macos")]
        let n = unsafe { libc::flistxattr(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        // SAFETY: as above.
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::flistxattr(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            break usize::try_from(n).unwrap_or(0);
        }
        match last() {
            ERANGE if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            e => return Err(e),
        }
    };
    let mut out = Vec::new();
    for name in buf
        .get(..n)
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
    {
        let ours =
            name == OWNER_XATTR.as_bytes() || name.strip_prefix(b"user.") == Some(OWNER_XATTR.as_bytes());
        let Some(name) = guest_xattr(name).filter(|_| !ours) else {
            continue;
        };
        let linux = [&b"user."[..], b"trusted.", b"security.", b"system."]
            .iter()
            .any(|p| name.starts_with(p));
        if linux {
            out.extend_from_slice(name);
            out.push(0);
        }
    }
    Ok(out)
}

fn pread(fd: host::Raw, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
    let mut done = 0;
    while done < buf.len() {
        let rest = buf.get_mut(done..).ok_or(EIO)?;
        // SAFETY: pread(2) into the rest of our buffer.
        let n = unsafe {
            libc::pread(
                fd,
                rest.as_mut_ptr().cast(),
                rest.len(),
                (offset + done as u64) as host::off_t,
            )
        };
        match n {
            0 => break,
            n if n > 0 => done += n as usize,
            _ if last() == 4 => {}
            _ => return Err(last()),
        }
    }
    Ok(done)
}

/// Drops the host's cached pages of `len` bytes of `fd` at `offset`, clean ones that no
/// process maps (posix_fadvise POSIX_FADV_DONTNEED), as the guest took them: a hint, which
/// a host without it ignores.
fn drop_cached(fd: host::Raw, offset: u64, len: usize) {
    #[cfg(target_os = "linux")]
    {
        let (Ok(offset), Ok(len)) = (host::off_t::try_from(offset), host::off_t::try_from(len)) else {
            return;
        };
        // SAFETY: advice on a range of a descriptor the server holds.
        unsafe { libc::posix_fadvise(fd, offset, len, libc::POSIX_FADV_DONTNEED) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (fd, offset, len);
}

fn pwrite(fd: host::Raw, buf: &[u8], offset: u64) -> Result<usize, Errno> {
    let mut done = 0;
    while done < buf.len() {
        let rest = buf.get(done..).ok_or(EIO)?;
        // SAFETY: pwrite(2) of the rest of the guest's data.
        let n = unsafe {
            libc::pwrite(
                fd,
                rest.as_ptr().cast(),
                rest.len(),
                (offset + done as u64) as host::off_t,
            )
        };
        match n {
            n if n > 0 => done += n as usize,
            0 => break,
            _ if last() == 4 => {}
            _ => return Err(last()),
        }
    }
    Ok(done)
}

/// Durable on stable storage: F_FULLFSYNC on macOS, whose fsync leaves the drive's cache.
fn durable(fd: host::Raw) -> Result<(), Errno> {
    #[cfg(target_os = "macos")]
    // SAFETY: fcntl(2) of a descriptor we hold.
    let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
    #[cfg(target_os = "linux")]
    // SAFETY: fsync(2) of a descriptor we hold.
    let rc = unsafe { libc::fsync(fd) };
    if rc != 0 {
        return Err(last());
    }
    Ok(())
}

/// fallocate(2)'s allocation, with or without KEEP_SIZE; nothing else Linux's takes.
fn fallocate(fd: host::Raw, mode: u32, offset: u64, length: u64) -> Result<(), Errno> {
    if mode & !linux::FALLOC_FL_KEEP_SIZE != 0 {
        return Err(EOPNOTSUPP);
    }
    let end = offset.checked_add(length).ok_or(EINVAL)?;
    #[cfg(target_os = "linux")]
    {
        let _ = end;
        // SAFETY: fallocate(2) of a descriptor we hold.
        if unsafe {
            libc::fallocate(
                fd,
                mode as host::c_int,
                offset as host::off_t,
                length as host::off_t,
            )
        } != 0
        {
            return Err(last());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let size = stat_fd(fd)?.size;
        if end > size {
            let mut store = libc::fstore_t {
                fst_flags: libc::F_ALLOCATEALL,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: (end - size) as host::off_t,
                fst_bytesalloc: 0,
            };
            // SAFETY: fcntl(2) F_PREALLOCATE with a store of ours.
            if unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut store) } != 0 {
                return Err(last());
            }
            if mode & linux::FALLOC_FL_KEEP_SIZE == 0 {
                // SAFETY: ftruncate(2) of a descriptor we hold.
                if unsafe { libc::ftruncate(fd, end as host::off_t) } != 0 {
                    return Err(last());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd as _, OwnedFd};

    /// A request: its header, then `body`.
    fn req(opcode: u32, nodeid: u64, uid: u32, body: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&u32::try_from(40 + body.len()).unwrap().to_le_bytes());
        r.extend_from_slice(&opcode.to_le_bytes());
        r.extend_from_slice(&7u64.to_le_bytes());
        r.extend_from_slice(&nodeid.to_le_bytes());
        r.extend_from_slice(&uid.to_le_bytes());
        r.extend_from_slice(&uid.to_le_bytes());
        r.extend_from_slice(&1u32.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(body);
        r
    }

    fn name(n: &str) -> Vec<u8> {
        let mut v = n.as_bytes().to_vec();
        v.push(0);
        v
    }

    /// The reply's error and payload.
    fn answer(server: &Server, r: &[u8]) -> (i32, Vec<u8>) {
        let out = server.handle(r).unwrap();
        let err = i32::from_le_bytes(out[4..8].try_into().unwrap());
        (err, out[16..].to_vec())
    }

    /// A share of a directory of the test's own, removed as it goes.
    fn dir() -> (shards_testdir::TempDir, Server) {
        let p = shards_testdir::TempDir::new("fs").unwrap();
        let fd = std::fs::File::open(&p).unwrap();
        let server = Server::new(fd.into(), false, None).unwrap();
        (p, server)
    }

    /// DAX_SMALLEST is what PM M338's measurement found by its rule: 1 MiB, the smallest
    /// measured size from which a window was no worse than FUSE on the Mac (from 256 KiB)
    /// and on Fedora's performance cores (from 1 MiB) and efficient ones (from 256 KiB).
    #[test]
    fn dax_smallest_is_what_its_measurement_found() {
        assert_eq!(DAX_SMALLEST, 1 << 20, "PM M338");
    }

    /// A guest that maps files into a DAX window (D189) is told the host's page size its
    /// mappings' offsets are whole multiples of, and which files to map: a regular file of
    /// `DAX_SMALLEST` bytes or more the server may read, never one a byte short, and where a
    /// mapping's descriptor is opened to write too (Hypervisor.framework's), one it may
    /// write, in a share written to. A mapping's file, which the guest names by its node
    /// alone, is opened to read, and to write too where the share is written to; never a
    /// directory, a node the server does not know, or a file put at its name since.
    #[test]
    fn dax_is_offered_for_files_a_window_maps() {
        let (p, s) = dir();
        std::fs::write(p.join("big"), vec![1u8; DAX_SMALLEST as usize]).unwrap();
        std::fs::write(p.join("small"), vec![1u8; DAX_SMALLEST as usize - 1]).unwrap();
        std::fs::write(p.join("kept"), vec![1u8; DAX_SMALLEST as usize]).unwrap();
        std::fs::set_permissions(
            p.join("kept"),
            std::os::unix::fs::PermissionsExt::from_mode(0o444),
        )
        .unwrap();
        let mut init = Vec::new();
        for word in [
            7u32,
            45,
            0,
            init::MAP_ALIGNMENT | init::INIT_EXT,
            init::HAS_INODE_DAX,
        ] {
            init.extend_from_slice(&word.to_le_bytes());
        }
        init.extend_from_slice(&[0u8; 44]);
        let (err, out) = answer(&s, &req(op::INIT, ROOT, 0, &init));
        assert_eq!(err, 0);
        let flags = u32::from_le_bytes(out[12..16].try_into().unwrap());
        let alignment = u16::from_le_bytes(out[30..32].try_into().unwrap());
        let flags2 = u32::from_le_bytes(out[32..36].try_into().unwrap());
        // SAFETY: sysconf(3) of a constant.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        assert_ne!(flags & init::MAP_ALIGNMENT, 0);
        assert_eq!(u32::from(alignment), page.trailing_zeros());
        assert_ne!(flags2 & init::HAS_INODE_DAX, 0);
        // fuse_entry_out: the node, then the attribute after 40 bytes; its flags its last.
        let looked = |n: &str| {
            let (err, out) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name(n)));
            assert_eq!(err, 0);
            let node = u64::from_le_bytes(out[0..8].try_into().unwrap());
            (node, u32::from_le_bytes(out[124..128].try_into().unwrap()))
        };
        let (big, big_flags) = looked("big");
        let (_, small_flags) = looked("small");
        let (_, kept_flags) = looked("kept");
        assert_eq!(big_flags & ATTR_DAX, ATTR_DAX);
        assert_eq!(small_flags & ATTR_DAX, 0);
        let read_alone = if super::super::window::SHARED_NEEDS_WRITE {
            0
        } else {
            ATTR_DAX
        };
        assert_eq!(kept_flags & ATTR_DAX, read_alone);
        let mut first = [0u8];
        let read = std::fs::File::from(s.descriptor(big, false).unwrap());
        crate::platform::read_exact_at(&read, &mut first, 0).unwrap();
        assert_eq!(first, [1]);
        let written = std::fs::File::from(s.descriptor(big, true).unwrap());
        crate::platform::write_all_at(&written, &[2], 0).unwrap();
        assert_eq!(std::fs::read(p.join("big")).unwrap()[0], 2);
        assert_eq!(s.descriptor(ROOT, false).err(), Some(EISDIR));
        assert_eq!(s.descriptor(999, false).err(), Some(ENOENT));
        // A read-only share: its file read alone, never written; where a mapping's
        // descriptor is opened to write too, neither, and no file is mapped.
        let ro = Server::new(std::fs::File::open(&p).unwrap().into(), true, None).unwrap();
        let (err, out) = answer(&ro, &req(op::LOOKUP, ROOT, 0, &name("big")));
        assert_eq!(err, 0);
        let node = u64::from_le_bytes(out[0..8].try_into().unwrap());
        let flags = u32::from_le_bytes(out[124..128].try_into().unwrap());
        assert_eq!(flags & ATTR_DAX, read_alone);
        assert_eq!(ro.descriptor(node, false).is_ok(), read_alone != 0);
        assert_eq!(ro.descriptor(node, true).err(), Some(EROFS));
        // Another file put at its name on the host: stale, the guest's kernel looks again.
        std::fs::rename(p.join("small"), p.join("big")).unwrap();
        assert_eq!(s.descriptor(big, false).err(), Some(ESTALE));
    }

    /// A join share's volumes (D119): each reached from the share's empty root by its name
    /// alone, nothing made in the root; each with its own files; a read-only one refusing
    /// writes; one directory shared as two volumes, read-only and not, two nodes, so that the
    /// read-only one's refuses what the other's takes; nothing moved between volumes; one
    /// taken out stale.
    #[test]
    fn a_join_share_serves_each_volume_alone() {
        let (root, _) = dir();
        let (a, _) = dir();
        let (b, _) = dir();
        std::fs::write(a.join("x"), "x").unwrap();
        std::fs::write(b.join("y"), "y").unwrap();
        let fd = |p: &std::path::Path| -> OwnedFd { std::fs::File::open(p).unwrap().into() };
        let c = |n: &str| CString::new(n).unwrap();
        let s = Server::joined(fd(&root), 64).unwrap();
        s.add(c("a"), fd(&a), false, None).unwrap();
        s.add(c("b"), fd(&b), false, None).unwrap();
        s.add(c("aro"), fd(&a), true, None).unwrap();
        assert!(s.add(c("a"), fd(&b), false, None).is_err());
        assert!(s.add(c("../up"), fd(&b), false, None).is_err());
        let node = |r: (i32, Vec<u8>)| {
            assert_eq!(r.0, 0);
            u64::from_le_bytes(r.1[0..8].try_into().unwrap())
        };
        let va = node(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("a"))));
        let vb = node(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("b"))));
        let vro = node(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("aro"))));
        assert_eq!(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("nope"))).0, -ENOENT);
        assert_eq!(answer(&s, &req(op::LOOKUP, va, 0, &name("y"))).0, -ENOENT);
        let mkdir = |n: &str| [0u8; 8].iter().copied().chain(name(n)).collect::<Vec<_>>();
        assert_eq!(answer(&s, &req(op::MKDIR, ROOT, 0, &mkdir("d"))).0, -EACCES);
        assert_eq!(answer(&s, &req(op::MKDIR, vro, 0, &mkdir("d"))).0, -EROFS);
        assert_eq!(answer(&s, &req(op::MKDIR, va, 0, &mkdir("d"))).0, 0);
        assert!(a.join("d").is_dir());
        let xa = node(answer(&s, &req(op::LOOKUP, va, 0, &name("x"))));
        let xro = node(answer(&s, &req(op::LOOKUP, vro, 0, &name("x"))));
        assert_ne!(xa, xro);
        // OPEN for writing (O_WRONLY).
        let open = |n: u64| answer(&s, &req(op::OPEN, n, 0, &[1, 0, 0, 0, 0, 0, 0, 0])).0;
        assert_eq!(open(xro), -EROFS);
        assert_eq!(open(xa), 0);
        let mut moved = vb.to_le_bytes().to_vec();
        moved.extend(name("x"));
        moved.extend(name("x2"));
        assert_eq!(answer(&s, &req(op::RENAME, va, 0, &moved)).0, -EXDEV);
        assert!(a.join("x").is_file() && !b.join("x2").exists());
        s.remove(&c("b"));
        assert_eq!(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("b"))).0, -ENOENT);
        assert_eq!(answer(&s, &req(op::LOOKUP, vb, 0, &name("y"))).0, -ESTALE);
    }

    /// A snapshot keeps the share (D176): a server of the same directory, given what one
    /// saved, answers for the nodes and handles its guest holds, a directory's node and an
    /// open file's handle read on; only as the first thing it is told, and only of that
    /// directory. A file the guest holds open after removing its last name no path reaches,
    /// and no restore could open: its share is not saved.
    #[test]
    fn a_saved_share_is_served_again_by_a_restored_server() {
        let (root, s) = dir();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/f"), "kept bytes").unwrap();
        let id = |r: (i32, Vec<u8>)| {
            assert_eq!(r.0, 0);
            u64::from_le_bytes(r.1[0..8].try_into().unwrap())
        };
        let d = id(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("d"))));
        let f = id(answer(&s, &req(op::LOOKUP, d, 0, &name("f"))));
        let fh = id(answer(&s, &req(op::OPEN, f, 0, &[0; 8])));
        // fuse_read_in: fh, offset, size, read_flags, lock_owner, flags, padding.
        let read = |s: &Server| {
            let mut body = fh.to_le_bytes().to_vec();
            body.extend(0u64.to_le_bytes());
            body.extend(64u32.to_le_bytes());
            body.extend([0u8; 4 + 8 + 4 + 4]);
            answer(s, &req(op::READ, f, 0, &body))
        };
        assert_eq!(read(&s), (0, b"kept bytes".to_vec()));
        let tables = s.save().unwrap();
        let again = Server::new(std::fs::File::open(&root).unwrap().into(), false, None).unwrap();
        again.restore(&tables).unwrap();
        assert_eq!(read(&again), (0, b"kept bytes".to_vec()));
        assert_eq!(id(answer(&again, &req(op::LOOKUP, d, 0, &name("f")))), f);
        std::fs::write(root.join("d/g"), "new").unwrap();
        let g = id(answer(&again, &req(op::LOOKUP, d, 0, &name("g"))));
        assert!(g != f && g != d && g != ROOT);
        assert!(again.restore(&tables).is_err(), "taken only first");
        // Saved where the host numbered the directory's device otherwise, as its boot may:
        // the nodes of its device are taken as of the device now.
        let dev = s.state.lock().unwrap().nodes[&ROOT].key.0;
        let (from, to) = (dev.to_le_bytes(), (dev ^ 0x5a5a).to_le_bytes());
        let mut renumbered = tables.clone();
        let (mut at, mut moved) = (0, 0);
        while at + 8 <= renumbered.len() {
            if renumbered[at..at + 8] == from {
                renumbered[at..at + 8].copy_from_slice(&to);
                moved += 1;
                at += 8;
            } else {
                at += 1;
            }
        }
        assert!(moved >= 4, "the root's and each node's: {moved}");
        let rebooted = Server::new(std::fs::File::open(&root).unwrap().into(), false, None).unwrap();
        rebooted.restore(&renumbered).unwrap();
        assert_eq!(read(&rebooted), (0, b"kept bytes".to_vec()));
        let (other, _) = dir();
        let elsewhere = Server::new(std::fs::File::open(&other).unwrap().into(), false, None).unwrap();
        assert!(
            elsewhere
                .restore(&tables)
                .unwrap_err()
                .contains("not the directory it was saved with"),
            "another directory's"
        );
        assert!(elsewhere.restore(&tables[..tables.len() - 1]).is_err());
        // A file the guest holds open after removing its last name no path reaches: its
        // bytes go with the tables, and a restored server holds them in a file of its own,
        // made in its private directory and removed there at once.
        std::fs::write(root.join("h"), "held after its removal").unwrap();
        let h = id(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("h"))));
        let hh = id(answer(&s, &req(op::OPEN, h, 0, &[0; 8])));
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("h"))).0, 0);
        let read_h = |s: &Server| {
            let mut body = hh.to_le_bytes().to_vec();
            body.extend(0u64.to_le_bytes());
            body.extend(64u32.to_le_bytes());
            body.extend([0u8; 4 + 8 + 4 + 4]);
            answer(s, &req(op::READ, h, 0, &body))
        };
        assert_eq!(read_h(&s), (0, b"held after its removal".to_vec()));
        let tables = s.save().unwrap();
        let private = shards_testdir::TempDir::new("fs-ghosts").unwrap();
        let held = Server::new(std::fs::File::open(&root).unwrap().into(), false, None).unwrap();
        held.keep_ghosts_in(std::fs::File::open(&private).unwrap().into());
        held.restore(&tables).unwrap();
        assert_eq!(read_h(&held), (0, b"held after its removal".to_vec()));
        assert!(
            std::fs::read_dir(&private).unwrap().next().is_none(),
            "no name left"
        );
        assert_eq!(read(&held), (0, b"kept bytes".to_vec()));
        let nowhere = Server::new(std::fs::File::open(&root).unwrap().into(), false, None).unwrap();
        assert!(
            nowhere
                .restore(&tables)
                .unwrap_err()
                .contains("nowhere to keep them")
        );
        // More than a snapshot's state holds refuses the save, before a byte is read.
        std::fs::write(root.join("big"), "").unwrap();
        let big = id(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("big"))));
        assert_eq!(answer(&s, &req(op::OPEN, big, 0, &[0; 8])).0, 0);
        std::fs::File::options()
            .write(true)
            .open(root.join("big"))
            .unwrap()
            .set_len(MAX_TABLES as u64 + 1)
            .unwrap();
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("big"))).0, 0);
        assert!(s.save().unwrap_err().contains("more than a snapshot keeps"));
    }

    /// Node IDs are not guessed from one another: one file's next to the last's is no
    /// neighbour of it, and two servers of one directory number its files apart, so that a
    /// handle to a node no path reaches cannot be forged (open_by_handle_at).
    #[test]
    fn node_ids_are_not_guessed_from_one_another() {
        let (root, s) = dir();
        for n in ["a", "b", "c"] {
            std::fs::write(root.join(n), n).unwrap();
        }
        let other = Server::new(std::fs::File::open(&root).unwrap().into(), false, None).unwrap();
        let ids = |s: &Server| -> Vec<u64> {
            ["a", "b", "c"]
                .iter()
                .map(|n| {
                    let (e, entry) = answer(s, &req(op::LOOKUP, ROOT, 0, &name(n)));
                    assert_eq!(e, 0);
                    u64::from_le_bytes(entry[0..8].try_into().unwrap())
                })
                .collect()
        };
        let (mine, theirs) = (ids(&s), ids(&other));
        for pair in mine.windows(2) {
            assert!(pair[0].abs_diff(pair[1]) > 1 << 16, "{mine:?}");
        }
        assert!(mine.iter().all(|id| !theirs.contains(id)), "{mine:?} {theirs:?}");
    }

    /// The end of a session (DESTROY, or the next INIT) forgets every node and handle the
    /// guest's kernel had, which it forgets without FORGET as its share's last mount goes;
    /// a join share's volumes stay, found again by name.
    #[test]
    fn a_session_that_ends_leaves_nothing_held() {
        let (root, _) = dir();
        let (a, _) = dir();
        std::fs::create_dir_all(a.join("d/e")).unwrap();
        std::fs::write(a.join("d/e/f"), "f").unwrap();
        let fd = |p: &std::path::Path| -> OwnedFd { std::fs::File::open(p).unwrap().into() };
        let s = Server::joined(fd(&root), 64).unwrap();
        s.add(CString::new("a").unwrap(), fd(&a), false, None).unwrap();
        let node = |r: (i32, Vec<u8>)| {
            assert_eq!(r.0, 0);
            u64::from_le_bytes(r.1[0..8].try_into().unwrap())
        };
        let held = |s: &Server| {
            let st = s.state.lock().unwrap();
            (
                st.nodes.len(),
                st.by_key.len(),
                st.handles.len(),
                st.fds.open.len(),
            )
        };
        let session = |s: &Server| {
            let v = node(answer(s, &req(op::LOOKUP, ROOT, 0, &name("a"))));
            let d = node(answer(s, &req(op::LOOKUP, v, 0, &name("d"))));
            let e = node(answer(s, &req(op::LOOKUP, d, 0, &name("e"))));
            let f = node(answer(s, &req(op::LOOKUP, e, 0, &name("f"))));
            assert_eq!(answer(s, &req(op::OPEN, f, 0, &[0; 8])).0, 0);
            assert_eq!(answer(s, &req(op::OPENDIR, e, 0, &[0; 8])).0, 0);
            f
        };
        let before = held(&s);
        let f = session(&s);
        assert!(held(&s).0 > before.0 && held(&s).2 == 2, "{:?}", held(&s));
        assert_eq!(answer(&s, &req(op::DESTROY, ROOT, 0, &[])).0, 0);
        assert_eq!(held(&s), (before.0, before.1, 0, 0));
        assert_eq!(answer(&s, &req(op::GETATTR, f, 0, &[0; 16])).0, -ENOENT);
        // A second session finds its volume again, then ends with an INIT alone.
        session(&s);
        let init = [7u32, 45, 0, 0]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(answer(&s, &req(op::INIT, ROOT, 0, &init)).0, 0);
        assert_eq!(held(&s), (before.0, before.1, 0, 0));
    }

    /// A join share's volume of one name, a file a joiner binds alone, shows that file alone
    /// of its directory, as a share of one name does.
    #[test]
    fn a_join_shares_volume_of_one_name_shows_it_alone() {
        let (root, _) = dir();
        let (p, _) = dir();
        std::fs::write(p.join("bound"), "b").unwrap();
        std::fs::write(p.join("secret"), "s").unwrap();
        let fd = |p: &std::path::Path| -> OwnedFd { std::fs::File::open(p).unwrap().into() };
        let s = Server::joined(fd(&root), 64).unwrap();
        s.add(
            CString::new("f").unwrap(),
            fd(&p),
            false,
            Some(CString::new("bound").unwrap()),
        )
        .unwrap();
        let (e, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("f")));
        assert_eq!(e, 0);
        let v = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        assert_eq!(answer(&s, &req(op::LOOKUP, v, 0, &name("bound"))).0, 0);
        assert_eq!(answer(&s, &req(op::LOOKUP, v, 0, &name("secret"))).0, -ENOENT);
        // Listed, the volume shows its one name, and the share's root none of the files
        // its directory has.
        std::fs::write(root.join("stray"), "s").unwrap();
        for (dir, want) in [(v, vec!["bound"]), (ROOT, vec![])] {
            for opcode in [op::READDIR, op::READDIRPLUS] {
                let (e, opened) = answer(&s, &req(op::OPENDIR, dir, 0, &[0; 8]));
                assert_eq!(e, 0);
                let fh = u64::from_le_bytes(opened[0..8].try_into().unwrap());
                let (e, listing) = answer(&s, &req(opcode, dir, 0, &readdir(fh, 0, 4096)));
                assert_eq!(e, 0);
                let mut names: Vec<String> = page(&listing, opcode == op::READDIRPLUS)
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                names.retain(|n| n != "." && n != "..");
                assert_eq!(names, want, "{opcode}");
            }
        }
        assert_eq!(answer(&s, &req(op::UNLINK, v, 0, &name("bound"))).0, -EBUSY);
        let mkdir = [0u8; 8].iter().copied().chain(name("d")).collect::<Vec<_>>();
        assert_eq!(answer(&s, &req(op::MKDIR, v, 0, &mkdir)).0, -EACCES);
    }

    #[test]
    fn a_file_bound_alone_is_all_of_its_directory_that_shows() {
        let (path, _) = dir();
        std::fs::write(path.join("bound"), "b").unwrap();
        std::fs::write(path.join("secret"), "s").unwrap();
        let fd = std::fs::File::open(&path).unwrap();
        let s = Server::new(fd.into(), false, Some(CString::new("bound").unwrap())).unwrap();
        assert_eq!(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("bound"))).0, 0);
        assert_eq!(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("secret"))).0, -ENOENT);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("bound"))).0, -EBUSY);
        assert_eq!(
            answer(
                &s,
                &req(
                    op::MKDIR,
                    ROOT,
                    0,
                    &[0u8; 8].iter().copied().chain(name("d")).collect::<Vec<_>>()
                )
            )
            .0,
            -EACCES
        );
        // Its directory lists the file alone.
        let (e, open) = answer(&s, &req(op::OPENDIR, ROOT, 0, &[0u8; 8]));
        assert_eq!(e, 0);
        let mut body = open[0..8].to_vec();
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&4096u32.to_le_bytes());
        body.extend_from_slice(&[0u8; 12]);
        let (e, listed) = answer(&s, &req(op::READDIR, ROOT, 0, &body));
        assert_eq!(e, 0);
        let text = String::from_utf8_lossy(&listed);
        assert!(text.contains("bound") && !text.contains("secret"), "{text:?}");
    }

    #[test]
    fn files_are_made_written_read_and_owned_as_a_guest_sees_them() {
        let (path, s) = dir();
        std::fs::write(path.join("host.txt"), "hi").unwrap();
        // LOOKUP: a host file is root's.
        let (e, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("host.txt")));
        assert_eq!(e, 0);
        let node = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let attr = &entry[40..];
        let size = u64::from_le_bytes(attr[8..16].try_into().unwrap());
        let uid = u32::from_le_bytes(attr[68..72].try_into().unwrap());
        assert_eq!((size, uid), (2, 0));
        // CREATE as uid 1000: theirs.
        let mut body = Vec::new();
        for v in [0o102u32 /* O_RDWR|O_CREAT */, 0o100644, 0o022, 0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend(name("made"));
        let (e, made) = answer(&s, &req(op::CREATE, ROOT, 1000, &body));
        assert_eq!(e, 0, "create");
        let made_uid = u32::from_le_bytes(made[40 + 68..40 + 72].try_into().unwrap());
        assert_eq!(made_uid, 1000);
        let fh = u64::from_le_bytes(made[128..136].try_into().unwrap());
        // WRITE then READ.
        let mut w = Vec::new();
        w.extend_from_slice(&fh.to_le_bytes());
        w.extend_from_slice(&0u64.to_le_bytes());
        w.extend_from_slice(&5u32.to_le_bytes());
        w.extend_from_slice(&[0u8; 20]);
        w.extend_from_slice(b"hello");
        let (e, written) = answer(&s, &req(op::WRITE, 0, 1000, &w));
        assert_eq!((e, u32::from_le_bytes(written[0..4].try_into().unwrap())), (0, 5));
        let mut rd = Vec::new();
        rd.extend_from_slice(&fh.to_le_bytes());
        rd.extend_from_slice(&1u64.to_le_bytes());
        rd.extend_from_slice(&100u32.to_le_bytes());
        rd.extend_from_slice(&[0u8; 20]);
        let (e, data) = answer(&s, &req(op::READ, 0, 1000, &rd));
        assert_eq!((e, data.as_slice()), (0, &b"ello"[..]));
        assert_eq!(std::fs::read(path.join("made")).unwrap(), b"hello");
        // The owner's attribute is hidden from the guest's list.
        let mut lx = Vec::new();
        lx.extend_from_slice(&4096u32.to_le_bytes());
        lx.extend_from_slice(&0u32.to_le_bytes());
        let (e, names) = answer(&s, &req(op::LISTXATTR, node, 0, &lx));
        assert_eq!((e, names.len()), (0, 0));
        // Names that escape are refused.
        assert_eq!(answer(&s, &req(op::LOOKUP, ROOT, 0, &name(".."))).0, -EINVAL);
        assert_eq!(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("a/b"))).0, -EINVAL);
        assert_eq!(answer(&s, &req(op::LOOKUP, ROOT, 0, &name("nope"))).0, -ENOENT);
        // FORGET takes no reply.
        assert!(s.handle(&req(op::FORGET, node, 0, &1u64.to_le_bytes())).is_none());
        let _ = std::fs::remove_dir_all(&path);
    }

    /// struct fuse_setattr_in: `valid` and `mode`, everything else zero.
    fn setattr(valid: u32, mode: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&valid.to_le_bytes());
        b.extend_from_slice(&[0u8; 4 + 6 * 8 + 3 * 4]);
        b.extend_from_slice(&mode.to_le_bytes());
        b.extend_from_slice(&[0u8; 4 * 4]);
        b
    }

    /// A guest's mode change reaches nothing through a symlink: one the guest made to a
    /// host file outside the share leaves that file's mode as it was, as Linux changes no
    /// symlink's mode (EOPNOTSUPP); a regular file's still changes.
    #[test]
    fn a_mode_change_follows_no_symlink() {
        use std::os::unix::fs::PermissionsExt as _;
        let (path, s) = dir();
        let elsewhere = shards_testdir::TempDir::new("fs-outside").unwrap();
        let outside = elsewhere.join("outside");
        std::fs::write(&outside, "secret").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut body = name("link");
        body.extend(name(outside.to_str().unwrap()));
        assert_eq!(answer(&s, &req(op::SYMLINK, ROOT, 0, &body)).0, 0);
        let (e, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("link")));
        assert_eq!(e, 0);
        let link = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let mode = || std::fs::metadata(&outside).unwrap().permissions().mode() & 0o7777;
        for valid in [fattr::MODE, fattr::KILL_SUIDGID] {
            let (e, _) = answer(&s, &req(op::SETATTR, link, 0, &setattr(valid, 0o777)));
            assert_eq!(e, -EOPNOTSUPP, "valid {valid:#x}");
            assert_eq!(mode(), 0o600, "valid {valid:#x}: the file outside the share");
        }
        std::fs::write(path.join("file"), "f").unwrap();
        let (_, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("file")));
        let file = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        assert_eq!(
            answer(&s, &req(op::SETATTR, file, 0, &setattr(fattr::MODE, 0o640))).0,
            0
        );
        let changed = std::fs::metadata(path.join("file")).unwrap().permissions().mode();
        assert_eq!(changed & 0o7777, 0o640);
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&outside);
    }

    /// `request`'s errno, or none if it is still waiting after ten seconds, when the FIFO
    /// at `fifo` is opened at both ends to let it go.
    fn answered_or_stuck(s: &Server, request: &[u8], fifo: &std::path::Path) -> Option<i32> {
        let mut peer = None;
        let errno = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            scope.spawn(move || tx.send(answer(s, request).0));
            let errno = rx.recv_timeout(std::time::Duration::from_secs(10)).ok();
            if errno.is_none() {
                peer = std::fs::OpenOptions::new().read(true).write(true).open(fifo).ok();
            }
            errno
        });
        drop(peer);
        errno
    }

    /// Only regular files are opened (CVE-2020-35517; QEMU virtiofsd's `lo_inode_open`): a
    /// guest's OPEN or CREATE of a FIFO is refused at once, where the open waited on the
    /// FIFO's other end for good, and a device node a share reaches is never opened. A
    /// guest kernel sends neither, opening special files itself.
    #[test]
    fn special_files_are_never_opened() {
        let (path, s) = dir();
        let fields = |v: [u32; 4]| v.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        #[allow(clippy::unnecessary_cast)]
        let fifo_mode = libc::S_IFIFO as u32 | 0o644;
        let mut mknod = fields([fifo_mode, 0, 0, 0]);
        mknod.extend(name("fifo"));
        assert_eq!(answer(&s, &req(op::MKNOD, ROOT, 0, &mknod)).0, 0);
        let (_, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("fifo")));
        let fifo = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let open = req(op::OPEN, fifo, 0, &[0u8; 8]);
        let mut create = fields([1 /* O_WRONLY */, 0o644, 0, 0]);
        create.extend(name("fifo"));
        let create = req(op::CREATE, ROOT, 0, &create);
        for (what, request) in [("OPEN", &open), ("CREATE", &create)] {
            let errno = answered_or_stuck(&s, request, &path.join("fifo"));
            assert_eq!(errno, Some(-EBADF), "{what} of a FIFO");
        }
        let dev = Server::new(std::fs::File::open("/dev").unwrap().into(), true, None).unwrap();
        let (e, entry) = answer(&dev, &req(op::LOOKUP, ROOT, 0, &name("null")));
        assert_eq!(e, 0);
        let null = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        assert_eq!(answer(&dev, &req(op::OPEN, null, 0, &[0u8; 8])).0, -EBADF);
        // A regular file opens with the status flags asked for: opened without waiting,
        // it waits again unless the guest asked otherwise.
        std::fs::write(path.join("file"), "f").unwrap();
        let (_, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("file")));
        let file = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        for (guest, nonblocking) in [(linux::O_APPEND | 1, false), (linux::O_NONBLOCK, true)] {
            let (e, opened) = answer(&s, &req(op::OPEN, file, 0, &fields([guest, 0, 0, 0])[..8]));
            assert_eq!(e, 0);
            let fh = u64::from_le_bytes(opened[0..8].try_into().unwrap());
            let state = s.state.lock().unwrap();
            let Some((Held::Fd(fd), _)) = state.fds.open.get(&Slot::Handle(fh)) else {
                panic!("no file handle {fh}");
            };
            // SAFETY: F_GETFL of a descriptor the server holds.
            let status = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            assert_eq!(status & libc::O_NONBLOCK != 0, nonblocking, "{guest:#o}");
            assert_eq!(
                status & libc::O_APPEND != 0,
                guest & linux::O_APPEND != 0,
                "{guest:#o}"
            );
        }
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A READDIR body: `fh`, `offset` and `size`.
    fn readdir(fh: u64, offset: u64, size: u32) -> Vec<u8> {
        let mut b = fh.to_le_bytes().to_vec();
        b.extend_from_slice(&offset.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&[0u8; 12]);
        b
    }

    /// A directory's listing, however much the guest asks for, comes at most a largest
    /// read at a time: a reply larger than a frame was refused by the device, which then
    /// read the rest of it as the next replies.
    #[test]
    fn a_listing_comes_a_read_at_a_time() {
        let (path, s) = dir();
        // 5000 entries of 224 bytes: 1.1 MB, past a largest read and a frame.
        for i in 0..5000 {
            std::fs::write(path.join(format!("{i:0>200}")), "").unwrap();
        }
        let (_, open) = answer(&s, &req(op::OPENDIR, ROOT, 0, &[0u8; 8]));
        let fh = u64::from_le_bytes(open[0..8].try_into().unwrap());
        for opcode in [op::READDIR, op::READDIRPLUS] {
            let out = s
                .handle(&req(opcode, ROOT, 0, &readdir(fh, 0, u32::MAX)))
                .unwrap();
            assert!(
                out.len() - 16 <= MAX_WRITE as usize,
                "opcode {opcode}: {}",
                out.len()
            );
            assert!(out.len() <= super::super::MAX_FRAME, "opcode {opcode}");
        }
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A page of a listing: each entry's name and the offset that follows it (READDIRPLUS's
    /// with its entry before each).
    fn page(listed: &[u8], plus: bool) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut rest = listed;
        while !rest.is_empty() {
            let dirent = if plus { &rest[128..] } else { rest };
            let off = u64::from_le_bytes(dirent[8..16].try_into().unwrap());
            let len = u32::from_le_bytes(dirent[16..20].try_into().unwrap()) as usize;
            out.push((String::from_utf8(dirent[24..24 + len].to_vec()).unwrap(), off));
            let record = if plus { 152 } else { 24 } + len;
            rest = &rest[(record + 7) & !7..];
        }
        out
    }

    /// A directory read a page at a time, as a guest reads it, lists every entry once,
    /// two handles reading it in turns, one READDIR and one READDIRPLUS, each from where it
    /// left off; and lists it again from its start.
    #[test]
    fn a_directory_is_read_whole_a_page_at_a_time() {
        let (path, s) = dir();
        let mut all: Vec<String> = (0..300).map(|i| format!("entry-{i}")).collect();
        for name in &all {
            std::fs::write(path.join(name), "").unwrap();
        }
        all.extend([".".to_string(), "..".to_string()]);
        all.sort();
        let open = || {
            let (_, opened) = answer(&s, &req(op::OPENDIR, ROOT, 0, &[0u8; 8]));
            u64::from_le_bytes(opened[0..8].try_into().unwrap())
        };
        let readers = [(open(), op::READDIR), (open(), op::READDIRPLUS)];
        let mut offsets = [0u64; 2];
        let mut seen = [Vec::new(), Vec::new()];
        let mut first = Vec::new();
        while offsets.iter().any(|&o| o != u64::MAX) {
            for (i, &(fh, opcode)) in readers.iter().enumerate() {
                if offsets[i] == u64::MAX {
                    continue;
                }
                let (e, listed) = answer(&s, &req(opcode, ROOT, 0, &readdir(fh, offsets[i], 700)));
                assert_eq!(e, 0);
                let entries = page(&listed, opcode == op::READDIRPLUS);
                if first.is_empty() {
                    first = entries.clone();
                }
                offsets[i] = entries.last().map_or(u64::MAX, |&(_, off)| off);
                seen[i].extend(entries.into_iter().map(|(name, _)| name));
            }
        }
        for mut names in seen {
            names.sort();
            assert_eq!(names, all);
        }
        let (_, again) = answer(&s, &req(op::READDIR, ROOT, 0, &readdir(readers[0].0, 0, 700)));
        assert_eq!(page(&again, false), first);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A device node is never opened only to be looked at, which runs its driver's open
    /// where the guest only looked (a serial line raises DTR): a share of /dev looks up,
    /// stats and lists the attributes of `null` without opening it, and it has none.
    #[test]
    fn a_device_is_never_opened_to_be_looked_at() {
        let dev = Server::new(std::fs::File::open("/dev").unwrap().into(), true, None).unwrap();
        OPENED.with(|o| o.borrow_mut().clear());
        let (e, entry) = answer(&dev, &req(op::LOOKUP, ROOT, 0, &name("null")));
        assert_eq!(e, 0);
        let null = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        assert_eq!(answer(&dev, &req(op::GETATTR, null, 0, &[0u8; 16])).0, 0);
        let opened = OPENED.with(|o| o.borrow().clone());
        assert!(!opened.iter().any(|n| n.as_bytes() == b"null"), "{opened:?}");
        let mut get = 4096u32.to_le_bytes().to_vec();
        get.extend_from_slice(&[0u8; 4]);
        get.extend(name("user.a"));
        assert_eq!(answer(&dev, &req(op::GETXATTR, null, 0, &get)).0, -ENODATA);
        let mut list = 4096u32.to_le_bytes().to_vec();
        list.extend_from_slice(&[0u8; 4]);
        assert_eq!(answer(&dev, &req(op::LISTXATTR, null, 0, &list)), (0, Vec::new()));
        let opened = OPENED.with(|o| o.borrow().clone());
        assert!(!opened.iter().any(|n| n.as_bytes() == b"null"), "{opened:?}");
    }

    /// A server over `path` holding at most `budget` descriptors between requests.
    fn small(path: &std::path::Path, budget: u64) -> Server {
        let root = std::fs::File::open(path).unwrap();
        Server::with_budget(root.into(), false, None, budget).unwrap()
    }

    fn found(s: &Server, parent: u64, n: &str) -> u64 {
        let (e, entry) = answer(s, &req(op::LOOKUP, parent, 0, &name(n)));
        assert_eq!(e, 0, "LOOKUP {n}");
        u64::from_le_bytes(entry[0..8].try_into().unwrap())
    }

    /// A directory the server let go is found again along its path, which follows the
    /// guest's renames; one renamed or replaced on the host since, or reached only through a
    /// symlink, even one to itself, is stale (ESTALE), and nothing outside the share is
    /// reached through one (audit V09).
    #[test]
    fn a_directory_let_go_is_found_by_its_path_or_is_stale() {
        let (path, _) = dir();
        std::fs::create_dir_all(path.join("a/b")).unwrap();
        std::fs::create_dir_all(path.join("other")).unwrap();
        let elsewhere = shards_testdir::TempDir::new("fs-outside").unwrap();
        let outside = elsewhere.join("outside");
        std::fs::create_dir_all(outside.join("b")).unwrap();
        // A budget of one: each request on one directory lets the other go.
        let s = small(&path, 1);
        let a = found(&s, ROOT, "a");
        let b = found(&s, a, "b");
        let other = found(&s, ROOT, "other");
        let getattr = |n: u64| answer(&s, &req(op::GETATTR, n, 0, &[0u8; 16])).0;
        assert_eq!(getattr(b), 0);
        assert_eq!(getattr(other), 0);
        assert_eq!(getattr(b), 0, "along a/b");
        let mut rename = ROOT.to_le_bytes().to_vec();
        rename.extend(name("a"));
        rename.extend(name("z"));
        assert_eq!(answer(&s, &req(op::RENAME, ROOT, 0, &rename)).0, 0);
        assert_eq!(getattr(other), 0);
        assert_eq!(getattr(b), 0, "along z/b, the guest's rename");
        // Renamed on the host: stale.
        std::fs::rename(path.join("z"), path.join("y")).unwrap();
        assert_eq!(getattr(other), 0);
        assert_eq!(getattr(b), -ESTALE, "renamed on the host");
        // A symlink in its path, to the very directory it was: not followed.
        std::os::unix::fs::symlink("y", path.join("z")).unwrap();
        assert_eq!(getattr(b), -ESTALE, "through a symlink to itself");
        // A symlink out of the share: not followed, nothing made there.
        std::fs::remove_file(path.join("z")).unwrap();
        std::os::unix::fs::symlink(&outside, path.join("z")).unwrap();
        let mut mkdir = 0o755u32.to_le_bytes().to_vec();
        mkdir.extend_from_slice(&[0u8; 4]);
        mkdir.extend(name("made"));
        assert_eq!(answer(&s, &req(op::MKDIR, b, 0, &mkdir)).0, -ESTALE);
        assert!(!outside.join("b/made").exists(), "made outside the share");
        // Back at its path: found again.
        std::fs::remove_file(path.join("z")).unwrap();
        std::fs::rename(path.join("y"), path.join("z")).unwrap();
        assert_eq!(getattr(b), 0, "back along z/b");
        // Replaced on the host by another directory of its name: stale, and a lookup finds
        // the new one.
        assert_eq!(getattr(other), 0);
        std::fs::rename(path.join("z/b"), path.join("b.old")).unwrap();
        std::fs::create_dir(path.join("z/b")).unwrap();
        assert_eq!(getattr(b), -ESTALE, "replaced on the host");
        let z = found(&s, ROOT, "z");
        assert_ne!(found(&s, z, "b"), b);
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// What a share process's servers may hold, from this host's figures (2026-10-09):
    /// `kern.maxfilesperproc` 245,760; `kern.maxfiles` 491,520 with 10,754 open;
    /// `kern.maxprocperuid` 10,666, two processes a run that shares. Here the table binds:
    /// 90 a share process, its servers' part each.
    #[test]
    fn a_budget_is_reckoned_from_the_hosts_limits() {
        let mac = Limits {
            process: 245_760,
            held: 6,
            table: Some((491_520, 10_754)),
            processes: Some(10_666),
        };
        assert_eq!(mac.budget(1), 90);
        assert_eq!(mac.budget(3), 30);
        // A table without a bound, as systemd sets Linux's: the process's limit binds.
        let open_ended = Limits {
            table: Some((u64::MAX, 2_000)),
            ..mac
        };
        assert_eq!(open_ended.budget(1), 245_760 - 6 - REQUEST_FDS);
        // Where the system says nothing of its table or its processes.
        let unsaid = Limits {
            table: None,
            processes: None,
            ..mac
        };
        assert_eq!(unsaid.budget(2), (245_760 - 6 - 2 * REQUEST_FDS) / 2);
        let spent = Limits { process: 8, ..unsaid };
        assert_eq!(spent.budget(1), 0);
    }

    /// A reply is built where it is sent from: two READs of a MiB answered into one
    /// buffer, its bytes the file's, its header the reply's, the buffer never made anew
    /// (PM M314).
    #[test]
    fn reads_are_answered_into_the_buffer_their_asker_keeps() {
        let (p, s) = dir();
        let mib = 1usize << 20;
        let bytes: Vec<u8> = (0..2 * mib).map(|i| (i % 251) as u8).collect();
        std::fs::write(p.join("big"), &bytes).unwrap();
        let node = found(&s, ROOT, "big");
        let fh = open(&s, node, 0);
        let mut out = Vec::new();
        let mut at = None;
        for half in 0..2 {
            let mut b = fh.to_le_bytes().to_vec();
            b.extend_from_slice(&((half * mib) as u64).to_le_bytes());
            b.extend_from_slice(&u32::try_from(mib).unwrap().to_le_bytes());
            b.extend_from_slice(&[0u8; 20]);
            assert!(s.handle_into(&req(op::READ, node, 0, &b), &mut out));
            assert_eq!(
                u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize,
                16 + mib
            );
            assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), 0);
            assert!(out[16..] == bytes[half * mib..(half + 1) * mib], "half {half}");
            assert_eq!(
                *at.get_or_insert(out.as_ptr()),
                out.as_ptr(),
                "the buffer was made anew"
            );
        }
        // A request that takes no reply leaves the buffer to the next.
        let forget = req(op::FORGET, node, 0, &1u64.to_le_bytes());
        assert!(!s.handle_into(&forget, &mut out));
        assert!(out.capacity() >= 16 + mib);
    }

    /// A share whose budget has no room past what its guest reads drops its host's copy as
    /// the guest takes it, and one with room keeps it (D188, PM M318): the file's pages
    /// resident after each read (mincore), the budget a fake cgroup tree's, its room a GiB
    /// past every host's room for its reads.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_read_past_its_budget_drops_the_hosts_copy() {
        use std::os::fd::AsRawFd as _;
        let (p, s) = dir();
        // SAFETY: zeroed plain data, which statfs fills.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        let path = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: statfs(2) of a NUL-terminated path into a buffer of its size.
        assert_eq!(unsafe { libc::statfs(path.as_ptr(), &raw mut fs) }, 0);
        if i128::from(fs.f_type) == i128::from(libc::TMPFS_MAGIC) {
            eprintln!(
                "SKIP: {} is on tmpfs, whose clean pages are never dropped",
                p.display()
            );
            return;
        }
        let mib = 1usize << 20;
        let file = p.join("big");
        std::fs::write(&file, vec![7u8; 4 * mib]).unwrap();
        let held = std::fs::File::open(&file).unwrap();
        held.sync_all().unwrap();
        // The pages of the file's first `len` bytes resident in the host's page cache.
        let resident = || -> usize {
            let len = 4 * mib;
            // SAFETY: a read-only shared mapping of the file, which nothing else holds.
            let at = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    held.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(at, libc::MAP_FAILED);
            let mut vec = vec![0u8; len.div_ceil(4096)];
            // SAFETY: mincore(2) of the mapping into a byte for each of its pages.
            assert_eq!(unsafe { libc::mincore(at, len, vec.as_mut_ptr().cast()) }, 0);
            // SAFETY: the mapping made above.
            unsafe { libc::munmap(at, len) };
            vec.iter().filter(|&&b| b & 1 == 1).count()
        };
        let cgroups = |high: u64, current: u64| {
            let root = shards_testdir::TempDir::new("headroom-tree").unwrap();
            let slice = root.join("a.slice");
            std::fs::create_dir_all(&slice).unwrap();
            std::fs::write(slice.join("memory.high"), format!("{high}\n")).unwrap();
            std::fs::write(slice.join("memory.max"), "max\n").unwrap();
            std::fs::write(slice.join("memory.current"), format!("{current}\n")).unwrap();
            root
        };
        let node = found(&s, ROOT, "big");
        let fh = open(&s, node, 0);
        for (room, kept) in [(0, false), (1 << 30, true)] {
            drop_cached(held.as_raw_fd(), 0, 4 * mib);
            assert_eq!(resident(), 0, "the file's pages were not let go before the read");
            let tree = cgroups(2 << 30, (2 << 30) - room);
            s.drop_reads_past(super::super::headroom::Headroom::under(
                &tree,
                std::path::Path::new("a.slice"),
            ));
            for at in 0..4 {
                let (e, data) = read(&s, node, fh, (at * mib) as u64, u32::try_from(mib).unwrap());
                assert_eq!((e, data.len()), (0, mib));
            }
            let pages = resident();
            assert_eq!(
                pages > 0,
                kept,
                "with {room} bytes of room, {pages} pages of the read kept"
            );
        }
    }

    /// OPEN of `node` with guest `flags`: its handle.
    fn open(s: &Server, node: u64, flags: u32) -> u64 {
        let mut b = flags.to_le_bytes().to_vec();
        b.extend_from_slice(&[0u8; 4]);
        let (e, opened) = answer(s, &req(op::OPEN, node, 0, &b));
        assert_eq!(e, 0, "OPEN {node}");
        u64::from_le_bytes(opened[0..8].try_into().unwrap())
    }

    /// READ of `size` bytes at `offset` through handle `fh`: the error and the bytes.
    fn read(s: &Server, node: u64, fh: u64, offset: u64, size: u32) -> (i32, Vec<u8>) {
        let mut b = fh.to_le_bytes().to_vec();
        b.extend_from_slice(&offset.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&[0u8; 20]);
        answer(s, &req(op::READ, node, 0, &b))
    }

    /// WRITE of `data` at `offset` through handle `fh`: the error.
    fn write(s: &Server, node: u64, fh: u64, offset: u64, data: &[u8]) -> i32 {
        let mut b = fh.to_le_bytes().to_vec();
        b.extend_from_slice(&offset.to_le_bytes());
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        b.extend_from_slice(&[0u8; 20]);
        b.extend_from_slice(data);
        answer(s, &req(op::WRITE, node, 0, &b)).0
    }

    fn release(s: &Server, node: u64, fh: u64) {
        let mut b = fh.to_le_bytes().to_vec();
        b.extend_from_slice(&[0u8; 16]);
        assert_eq!(answer(s, &req(op::RELEASE, node, 0, &b)).0, 0);
    }

    /// The guest opens `n` other files, reads each and lets them go: more than the server
    /// has room for, so it lets go of the descriptors it held before.
    fn crowd(s: &Server, n: usize) {
        let held: Vec<(u64, u64)> = (0..n)
            .map(|i| {
                let node = found(s, ROOT, &format!("crowd{i}"));
                (node, open(s, node, 0))
            })
            .collect();
        for &(node, fh) in &held {
            assert_eq!(read(s, node, fh, 0, 8).0, 0);
        }
        for &(node, fh) in &held {
            release(s, node, fh);
        }
    }

    fn crowded(path: &std::path::Path, n: usize) {
        for i in 0..n {
            std::fs::write(path.join(format!("crowd{i}")), "c").unwrap();
        }
    }

    /// A guest holds open as many files as its own kernel lets it, past what the server may
    /// hold descriptors for, and reads each again and again: each handle's descriptor is
    /// opened again along its path when it is needed (audit V09).
    #[test]
    fn a_guest_reads_ten_thousand_files_it_holds_open() {
        let (path, _) = dir();
        for i in 0..10_000 {
            std::fs::write(path.join(format!("f{i}")), format!("{i}")).unwrap();
        }
        let s = small(&path, 90);
        let held: Vec<(u64, u64)> = (0..10_000)
            .map(|i| {
                let node = found(&s, ROOT, &format!("f{i}"));
                (node, open(&s, node, 0))
            })
            .collect();
        for _ in 0..2 {
            for (i, &(node, fh)) in held.iter().enumerate() {
                let (e, got) = read(&s, node, fh, 0, 16);
                assert_eq!(e, 0, "READ f{i}");
                assert_eq!(got, format!("{i}").as_bytes(), "f{i}");
            }
        }
        assert!(s.state.lock().unwrap().fds.open.len() <= 90 + REQUEST_FDS as usize);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A file the guest unlinks while it holds it open goes on being read and written through
    /// its handle, as on a kernel's own filesystem, however long its descriptor was let go:
    /// the server pins one for it before the unlink, and lets it go with its last handle.
    #[test]
    fn an_unlinked_file_held_open_goes_on_serving() {
        let (path, _) = dir();
        std::fs::write(path.join("a"), "before").unwrap();
        crowded(&path, 20);
        let s = small(&path, 4);
        let a = found(&s, ROOT, "a");
        let fh = open(&s, a, 2);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("a"))).0, 0);
        assert!(!path.join("a").exists());
        crowd(&s, 20);
        assert_eq!(write(&s, a, fh, 0, b"after!"), 0);
        crowd(&s, 20);
        assert_eq!(read(&s, a, fh, 0, 16), (0, b"after!".to_vec()));
        // fstat of it: through its handle, as there is no path to it.
        let mut getattr = 1u32.to_le_bytes().to_vec();
        getattr.extend_from_slice(&[0u8; 4]);
        getattr.extend_from_slice(&fh.to_le_bytes());
        assert_eq!(answer(&s, &req(op::GETATTR, a, 0, &getattr)).0, 0);
        assert_eq!(s.state.lock().unwrap().pins.len(), 1);
        release(&s, a, fh);
        assert_eq!(s.state.lock().unwrap().pins.len(), 0);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A file the guest renames another over while it holds it open goes on being read
    /// through its handle; its name opens the new one.
    #[test]
    fn a_file_renamed_over_while_open_goes_on_serving() {
        let (path, _) = dir();
        std::fs::write(path.join("a"), "old").unwrap();
        std::fs::write(path.join("b"), "new").unwrap();
        crowded(&path, 20);
        let s = small(&path, 4);
        let a = found(&s, ROOT, "a");
        let fh = open(&s, a, 0);
        let mut rename = ROOT.to_le_bytes().to_vec();
        rename.extend(name("b"));
        rename.extend(name("a"));
        assert_eq!(answer(&s, &req(op::RENAME, ROOT, 0, &rename)).0, 0);
        crowd(&s, 20);
        assert_eq!(read(&s, a, fh, 0, 16), (0, b"old".to_vec()));
        let now = found(&s, ROOT, "a");
        assert_ne!(now, a);
        let fresh = open(&s, now, 0);
        assert_eq!(read(&s, now, fresh, 0, 16), (0, b"new".to_vec()));
        // A handle opened to truncate is opened again without: what it wrote stays.
        std::fs::write(path.join("c"), "to be cut").unwrap();
        let c = found(&s, ROOT, "c");
        let fc = open(&s, c, 2 | linux::O_TRUNC);
        assert_eq!(write(&s, c, fc, 0, b"kept"), 0);
        crowd(&s, 20);
        assert_eq!(read(&s, c, fc, 0, 16), (0, b"kept".to_vec()));
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A file replaced on the host while the guest holds it open is read through its
    /// descriptor while the server holds it; once let go, the name leads to another file,
    /// and the handle is stale (ESTALE), as a stale NFS handle is. So is one removed on the
    /// host, or whose mode the host takes away, as none can be opened for it again.
    #[test]
    fn a_file_replaced_on_the_host_while_open_is_stale_once_let_go() {
        use std::os::unix::fs::PermissionsExt as _;
        let (path, _) = dir();
        std::fs::write(path.join("a"), "old").unwrap();
        crowded(&path, 20);
        let s = small(&path, 4);
        let a = found(&s, ROOT, "a");
        let fh = open(&s, a, 0);
        std::fs::write(path.join("a.new"), "new").unwrap();
        std::fs::rename(path.join("a.new"), path.join("a")).unwrap();
        assert_eq!(read(&s, a, fh, 0, 16), (0, b"old".to_vec()), "held still");
        crowd(&s, 20);
        assert_eq!(read(&s, a, fh, 0, 16).0, -ESTALE);
        // Removed on the host: stale too.
        let b = found(&s, ROOT, "a");
        let fb = open(&s, b, 0);
        std::fs::remove_file(path.join("a")).unwrap();
        crowd(&s, 20);
        assert_eq!(read(&s, b, fb, 0, 16).0, -ESTALE);
        // Its mode taken away on the host: stale, not refused.
        std::fs::write(path.join("c"), "c").unwrap();
        let c = found(&s, ROOT, "c");
        let fc = open(&s, c, 0);
        std::fs::set_permissions(path.join("c"), std::fs::Permissions::from_mode(0o200)).unwrap();
        crowd(&s, 20);
        assert_eq!(read(&s, c, fc, 0, 16).0, -ESTALE);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A directory's listing goes on where it was after its handle's stream is let go: each
    /// entry once, every page read after the server let go of everything it held.
    #[test]
    fn a_listing_goes_on_where_it_was_after_its_stream_is_let_go() {
        let (path, _) = dir();
        std::fs::create_dir(path.join("d")).unwrap();
        for i in 0..300 {
            std::fs::write(path.join(format!("d/e{i}")), "e").unwrap();
        }
        crowded(&path, 20);
        let s = small(&path, 4);
        let d = found(&s, ROOT, "d");
        let (e, opened) = answer(&s, &req(op::OPENDIR, d, 0, &[0u8; 8]));
        assert_eq!(e, 0);
        let fh = u64::from_le_bytes(opened[0..8].try_into().unwrap());
        let (mut names, mut offset, mut pages) = (Vec::new(), 0u64, 0);
        loop {
            let (e, listed) = answer(&s, &req(op::READDIR, d, 0, &readdir(fh, offset, 512)));
            assert_eq!(e, 0);
            let entries = page(&listed, false);
            let Some(&(_, last)) = entries.last() else {
                break;
            };
            names.extend(entries.into_iter().map(|(n, _)| n));
            offset = last;
            pages += 1;
            assert!(pages < 100, "the listing goes on past its entries");
            crowd(&s, 8);
        }
        assert!(pages > 10, "{pages} pages");
        let mut want: Vec<String> = (0..300).map(|i| format!("e{i}")).collect();
        want.extend([".".to_string(), "..".to_string()]);
        want.sort();
        names.sort();
        assert_eq!(names, want);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// Pins are the descriptors a server cannot let go, and only they count against its
    /// budget: past it, an unlink of a file the guest holds open goes on, and the handle is
    /// stale (ESTALE) instead of pinned; opens are never refused for it (no EMFILE).
    #[test]
    fn pins_past_the_budget_leave_their_handles_stale() {
        let (path, _) = dir();
        for i in 0..5 {
            std::fs::write(path.join(format!("p{i}")), format!("{i}")).unwrap();
        }
        crowded(&path, 50);
        let s = small(&path, 2);
        let held: Vec<(u64, u64)> = (0..5)
            .map(|i| {
                let node = found(&s, ROOT, &format!("p{i}"));
                (node, open(&s, node, 0))
            })
            .collect();
        for i in 0..5 {
            assert_eq!(
                answer(&s, &req(op::UNLINK, ROOT, 0, &name(&format!("p{i}")))).0,
                0
            );
        }
        crowd(&s, 50);
        // Between requests, what it holds, pins and all, is within its budget, however
        // many files the guest has open besides.
        let more: Vec<(u64, u64)> = (0..10)
            .map(|i| {
                let node = found(&s, ROOT, &format!("crowd{i}"));
                (node, open(&s, node, 0))
            })
            .collect();
        for &(node, fh) in &more {
            assert_eq!(read(&s, node, fh, 0, 8).0, 0);
        }
        let held_now = {
            let st = s.state.lock().unwrap();
            st.fds.open.len() + st.pins.len()
        };
        assert!(held_now <= 2, "{held_now} held");
        for &(node, fh) in &more {
            release(&s, node, fh);
        }
        for (i, &(node, fh)) in held.iter().enumerate() {
            let want = if i < 2 {
                (0, format!("{i}").into_bytes())
            } else {
                (-ESTALE, Vec::new())
            };
            assert_eq!(read(&s, node, fh, 0, 16), want, "p{i}");
        }
        // A pin let go with its last handle makes room for another.
        release(&s, held[0].0, held[0].1);
        std::fs::write(path.join("q"), "q").unwrap();
        let q = found(&s, ROOT, "q");
        let fq = open(&s, q, 0);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("q"))).0, 0);
        crowd(&s, 10);
        assert_eq!(read(&s, q, fq, 0, 16), (0, b"q".to_vec()));
        let _ = std::fs::remove_dir_all(&path);
    }

    /// CREATE of `n` in `parent` with guest `flags` (O_CREAT added) and `mode`: its node and
    /// handle.
    fn create(s: &Server, parent: u64, n: &str, flags: u32, mode: u32) -> (u64, u64) {
        let mut b = Vec::new();
        for v in [flags | linux::O_CREAT, 0o100_000 | mode, 0, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend(name(n));
        let (e, made) = answer(s, &req(op::CREATE, parent, 0, &b));
        assert_eq!(e, 0, "CREATE {n}");
        let node = u64::from_le_bytes(made[0..8].try_into().unwrap());
        (node, u64::from_le_bytes(made[128..136].try_into().unwrap()))
    }

    /// Linux's numbers for the host's refusals the tests expect.
    const EEXIST: i32 = 17;
    const ENOTEMPTY: i32 = 39;

    /// A file the guest makes read-only and fills through the handle that made it, as tar,
    /// cp and git make one (an exclusive create of mode 0444, then writes): no descriptor for
    /// writing could be opened for it again, so the one made is kept, and the handle writes
    /// however long since the server let go of everything else (audit V09).
    #[test]
    fn a_file_made_read_only_is_filled_through_its_handle() {
        use std::os::unix::fs::PermissionsExt as _;
        let (path, _) = dir();
        crowded(&path, 20);
        let s = small(&path, 4);
        let (node, fh) = create(&s, ROOT, "ro", 1 | linux::O_EXCL, 0o444);
        crowd(&s, 20);
        assert_eq!(write(&s, node, fh, 0, b"filled"), 0);
        crowd(&s, 20);
        assert_eq!(write(&s, node, fh, 6, b" twice"), 0);
        release(&s, node, fh);
        assert_eq!(std::fs::read(path.join("ro")).unwrap(), b"filled twice");
        // The host's file lets its owner, the share process, write it; the guest sees the
        // mode it made it with (D174).
        let mode = std::fs::metadata(path.join("ro")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        let (_, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("ro")));
        assert_eq!(
            u32::from_le_bytes(entry[40 + 60..40 + 64].try_into().unwrap()) & 0o7777,
            0o444
        );
        assert!(s.state.lock().unwrap().pins.is_empty(), "let go with its handle");
        let _ = std::fs::remove_dir_all(&path);
    }

    /// The guest's names for the host's attributes: its own `trusted.` and `security.` ones
    /// as the host keeps them (`user.shards.`), its `user.` ones as they are; and on Linux,
    /// none for the host's own `trusted.` and `security.` ones (an SELinux label), which the
    /// guest could list and never read; nor for the server's.
    #[test]
    fn the_guest_sees_its_own_attributes_and_not_the_hosts_security_ones() {
        assert_eq!(guest_xattr(b"user.x"), Some(&b"user.x"[..]));
        assert_eq!(guest_xattr(b"user.shards.other"), None);
        let host_label = guest_xattr(b"security.selinux");
        let host_trusted = guest_xattr(b"trusted.ima");
        if cfg!(target_os = "linux") {
            assert_eq!(
                guest_xattr(b"user.shards.security.capability"),
                Some(&b"security.capability"[..])
            );
            assert_eq!((host_label, host_trusted), (None, None));
        } else {
            assert_eq!(host_label, Some(&b"security.selinux"[..]));
        }
    }

    /// What the guest makes that the host cannot hold is kept beside it (D174), as a
    /// runtime nested in the guest (overlayfs, on a share as its upper layer) needs it:
    /// - a directory of mode 0, as overlayfs makes its work directory, is the guest's mode
    ///   0 and the host's 0700, its owner's to reach, and the guest root makes one in it;
    /// - a device node the host makes only for a privileged process, overlayfs's whiteout
    ///   of 0/0 among them, is to the guest the node it made, looked up and listed so; the
    ///   host keeps an empty regular file in its place where it could make no device;
    /// - a mode taken away leaves the host's owner its access, and the guest the mode;
    /// - an attribute of `trusted.`, which only a privileged process sets on Linux, is
    ///   kept, read and listed back under its own name.
    #[test]
    fn what_the_host_cannot_hold_is_kept_beside_it() {
        use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
        let (path, s) = dir();
        let mode_in = |entry: &[u8]| u32::from_le_bytes(entry[40 + 60..40 + 64].try_into().unwrap());
        let rdev_in = |entry: &[u8]| u32::from_le_bytes(entry[40 + 76..40 + 80].try_into().unwrap());
        let node_in = |entry: &[u8]| u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let fields = |v: &[u32]| v.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let host_mode = |name: &str| {
            std::fs::symlink_metadata(path.join(name))
                .unwrap()
                .permissions()
                .mode()
        };
        // A directory of mode 0, and one made in it by the guest's root.
        let mut mkdir = fields(&[0, 0]);
        mkdir.extend(name("work"));
        let (e, entry) = answer(&s, &req(op::MKDIR, ROOT, 0, &mkdir));
        assert_eq!(e, 0);
        assert_eq!(mode_in(&entry), S_IFDIR, "mode 0 to the guest");
        assert_eq!(host_mode("work") & 0o777, 0o700);
        let mut inner = fields(&[0, 0]);
        inner.extend(name("work"));
        assert_eq!(answer(&s, &req(op::MKDIR, node_in(&entry), 0, &inner)).0, 0);
        // A whiteout, and a device with a number (Linux's encoding of 1:3).
        for (file, rdev) in [("gone", 0u32), ("null", 0x103)] {
            let mut mknod = fields(&[S_IFCHR | 0o644, rdev, 0, 0]);
            mknod.extend(name(file));
            let (e, entry) = answer(&s, &req(op::MKNOD, ROOT, 0, &mknod));
            assert_eq!(e, 0, "{file}");
            assert_eq!(
                (mode_in(&entry), rdev_in(&entry)),
                (S_IFCHR | 0o644, rdev),
                "{file}"
            );
            let (_, looked) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name(file)));
            assert_eq!((mode_in(&looked), rdev_in(&looked)), (S_IFCHR | 0o644, rdev));
            // An unprivileged host keeps an empty regular file; a privileged one, the device.
            let kept = std::fs::symlink_metadata(path.join(file)).unwrap().file_type();
            assert!(kept.is_file() || kept.is_char_device(), "{file}: {kept:?}");
        }
        // Listed as the device it is, as overlayfs finds whiteouts in a listing.
        let (_, open) = answer(&s, &req(op::OPENDIR, ROOT, 0, &[0u8; 8]));
        let fh = u64::from_le_bytes(open[0..8].try_into().unwrap());
        let (e, listed) = answer(&s, &req(op::READDIR, ROOT, 0, &readdir(fh, 0, 4096)));
        assert_eq!(e, 0);
        let mut kinds = Vec::new();
        let mut rest = &listed[..];
        while !rest.is_empty() {
            let len = u32::from_le_bytes(rest[16..20].try_into().unwrap()) as usize;
            let kind = u32::from_le_bytes(rest[20..24].try_into().unwrap());
            kinds.push((String::from_utf8(rest[24..24 + len].to_vec()).unwrap(), kind));
            rest = &rest[(24 + len + 7) & !7..];
        }
        assert!(
            kinds.contains(&("gone".into(), u32::from(libc::DT_CHR))),
            "{kinds:?}"
        );
        // A mode taken away: the host's owner keeps its access, the guest has the mode.
        std::fs::write(path.join("file"), "f").unwrap();
        let (_, entry) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("file")));
        let file = node_in(&entry);
        let (e, changed) = answer(&s, &req(op::SETATTR, file, 0, &setattr(fattr::MODE, 0)));
        assert_eq!(e, 0);
        // fuse_attr_out: 16 bytes before its attribute, where an entry has 40.
        let changed_mode = u32::from_le_bytes(changed[16 + 60..16 + 64].try_into().unwrap());
        assert_eq!(changed_mode & 0o7777, 0, "the guest's mode");
        assert_eq!(host_mode("file") & 0o777, 0o600, "the host's");
        // An attribute of trusted., set, read and listed back.
        let mut set = fields(&[1, 0]);
        set.extend(name("trusted.overlay.opaque"));
        set.push(b'y');
        assert_eq!(answer(&s, &req(op::SETXATTR, file, 0, &set)).0, 0);
        let mut get = fields(&[64, 0]);
        get.extend(name("trusted.overlay.opaque"));
        assert_eq!(answer(&s, &req(op::GETXATTR, file, 0, &get)), (0, b"y".to_vec()));
        let (e, names) = answer(&s, &req(op::LISTXATTR, file, 0, &fields(&[4096, 0])));
        assert_eq!((e, names.as_slice()), (0, &b"trusted.overlay.opaque\0"[..]));
        // The server's own names are none of the guest's to set.
        let mut ours = fields(&[1, 0]);
        ours.extend(name("user.shards.trusted.x"));
        ours.push(b'y');
        assert_eq!(answer(&s, &req(op::SETXATTR, file, 0, &ours)).0, -EOPNOTSUPP);
        // overlayfs's shared whiteout: one made in its work directory, linked under another
        // name there, the link renamed into the upper directory, and looked up there.
        let mut wdir = fields(&[0o755, 0]);
        wdir.extend(name("wd"));
        let (_, wd) = answer(&s, &req(op::MKDIR, ROOT, 0, &wdir));
        let wd = node_in(&wd);
        let mut shared = fields(&[S_IFCHR, 0, 0, 0]);
        shared.extend(name("#1"));
        let (e, made) = answer(&s, &req(op::MKNOD, wd, 0, &shared));
        assert_eq!(e, 0);
        let mut link = node_in(&made).to_le_bytes().to_vec();
        link.extend(name("#2"));
        let (e, linked) = answer(&s, &req(op::LINK, wd, 0, &link));
        assert_eq!(e, 0, "a link of a whiteout");
        assert_eq!((mode_in(&linked), rdev_in(&linked)), (S_IFCHR, 0));
        let mut into = ROOT.to_le_bytes().to_vec();
        into.extend(fields(&[0, 0]));
        into.extend(name("#2"));
        into.extend(name("hidden"));
        assert_eq!(answer(&s, &req(op::RENAME2, wd, 0, &into)).0, 0);
        let (e, found) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("hidden")));
        assert_eq!((e, mode_in(&found), rdev_in(&found)), (0, S_IFCHR, 0));
        // The name the guest last knew it by unlinked, it is reached by another of its
        // names: looked at, and linked again, as overlayfs links its shared whiteout for
        // each removal and unlinks the links as it cleans a directory.
        let shared_node = node_in(&made);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("hidden"))).0, 0);
        let (e, attr) = answer(&s, &req(op::GETATTR, shared_node, 0, &[0u8; 16]));
        let attr_mode = u32::from_le_bytes(attr[16 + 60..16 + 64].try_into().unwrap());
        assert_eq!((e, attr_mode), (0, S_IFCHR), "reached by another name");
        let mut again = shared_node.to_le_bytes().to_vec();
        again.extend(name("#3"));
        assert_eq!(answer(&s, &req(op::LINK, wd, 0, &again)).0, 0, "linked again");
        // RENAME_WHITEOUT: the file renamed, a whiteout of mode 0 in its place.
        let mut whiteout = ROOT.to_le_bytes().to_vec();
        whiteout.extend(fields(&[linux::RENAME_WHITEOUT, 0]));
        whiteout.extend(name("file"));
        whiteout.extend(name("moved"));
        assert_eq!(answer(&s, &req(op::RENAME2, ROOT, 0, &whiteout)).0, 0);
        assert_eq!(std::fs::read(path.join("moved")).unwrap(), b"f");
        let (_, left) = answer(&s, &req(op::LOOKUP, ROOT, 0, &name("file")));
        assert_eq!((mode_in(&left), rdev_in(&left)), (S_IFCHR, 0));
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A file whose mode the guest takes away while it has the file open goes on being
    /// written and read through its handles, as on a kernel's own filesystem: before the
    /// change the server keeps a descriptor for them, as none could be opened after it. A
    /// new handle opens its own, with its own flags, where the mode lets it; and a change
    /// that refuses no handle keeps nothing.
    #[test]
    fn a_file_whose_mode_is_taken_away_goes_on_serving_its_handles() {
        let (path, _) = dir();
        std::fs::write(path.join("w"), "").unwrap();
        std::fs::write(path.join("r"), "read me").unwrap();
        std::fs::write(path.join("x"), "x").unwrap();
        crowded(&path, 20);
        let s = small(&path, 4);
        let chmod =
            |node: u64, mode: u32| answer(&s, &req(op::SETATTR, node, 0, &setattr(fattr::MODE, mode))).0;
        let w = found(&s, ROOT, "w");
        let fw = open(&s, w, 2);
        assert_eq!(chmod(w, 0o444), 0);
        crowd(&s, 20);
        assert_eq!(write(&s, w, fw, 0, b"still"), 0);
        crowd(&s, 20);
        assert_eq!(read(&s, w, fw, 0, 16), (0, b"still".to_vec()));
        // A reader, its file made write-only.
        let r = found(&s, ROOT, "r");
        let fr = open(&s, r, 0);
        assert_eq!(chmod(r, 0o200), 0);
        crowd(&s, 20);
        assert_eq!(read(&s, r, fr, 0, 16), (0, b"read me".to_vec()));
        // A new handle to `w` opens a descriptor of its own, with its own status flags.
        let fresh = open(&s, w, linux::O_NONBLOCK);
        {
            let state = s.state.lock().unwrap();
            let Some((Held::Fd(fd), _)) = state.fds.open.get(&Slot::Handle(fresh)) else {
                panic!("no descriptor of its own");
            };
            // SAFETY: F_GETFL of a descriptor the server holds.
            let status = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(status & libc::O_NONBLOCK, 0);
        }
        assert_eq!(read(&s, w, fresh, 0, 16), (0, b"still".to_vec()));
        // A mode that refuses none of its handles keeps nothing more.
        let x = found(&s, ROOT, "x");
        let fx = open(&s, x, 2);
        let pins = s.state.lock().unwrap().pins.len();
        assert_eq!(chmod(x, 0o600), 0);
        assert_eq!(s.state.lock().unwrap().pins.len(), pins);
        for (node, fh) in [(w, fw), (w, fresh), (r, fr), (x, fx)] {
            release(&s, node, fh);
        }
        assert!(s.state.lock().unwrap().pins.is_empty());
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A removal that fails takes back the pin it took for a node the guest has open, and,
    /// with no room for one, leaves its handles as they were: the node is still there, its
    /// path with it.
    #[test]
    fn a_removal_that_fails_leaves_open_nodes_as_they_were() {
        let (path, _) = dir();
        for n in ["x", "y", "p"] {
            std::fs::write(path.join(n), n).unwrap();
        }
        std::fs::create_dir(path.join("d")).unwrap();
        std::fs::write(path.join("d/inside"), "i").unwrap();
        crowded(&path, 20);
        let s = small(&path, 1);
        let noreplace = || {
            let mut b = ROOT.to_le_bytes().to_vec();
            b.extend_from_slice(&linux::RENAME_NOREPLACE.to_le_bytes());
            b.extend_from_slice(&[0u8; 4]);
            b.extend(name("y"));
            b.extend(name("x"));
            answer(&s, &req(op::RENAME2, ROOT, 0, &b)).0
        };
        let x = found(&s, ROOT, "x");
        let fx = open(&s, x, 0);
        assert_eq!(noreplace(), -EEXIST);
        assert!(s.state.lock().unwrap().pins.is_empty(), "taken back");
        // Its budget spent on a pin: the same, and x's handle reads on once let go.
        let p = found(&s, ROOT, "p");
        let _fp = open(&s, p, 0);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("p"))).0, 0);
        assert_eq!(noreplace(), -EEXIST);
        crowd(&s, 20);
        assert_eq!(read(&s, x, fx, 0, 8), (0, b"x".to_vec()));
        // A directory not empty: refused, and its handle lists on.
        let d = found(&s, ROOT, "d");
        let (e, opened) = answer(&s, &req(op::OPENDIR, d, 0, &[0u8; 8]));
        assert_eq!(e, 0);
        let fd = u64::from_le_bytes(opened[0..8].try_into().unwrap());
        assert_eq!(answer(&s, &req(op::RMDIR, ROOT, 0, &name("d"))).0, -ENOTEMPTY);
        crowd(&s, 20);
        let (e, listed) = answer(&s, &req(op::READDIR, d, 0, &readdir(fd, 0, 4096)));
        assert_eq!(e, 0);
        assert!(page(&listed, false).iter().any(|(n, _)| n == "inside"));
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A guest holds open as many files as its own kernel lets it, whatever the budget: a
    /// handle's descriptor is a cache the server lets go and opens again along its path,
    /// never a limit the guest meets (audit V09). A thousand handles past a budget of four,
    /// read round and round, each serves its own file, and the server holds no more than its
    /// budget's descriptors between requests.
    #[test]
    fn handles_past_the_budget_each_serve_their_file() {
        let (path, _) = dir();
        for i in 0..1000 {
            std::fs::write(path.join(format!("f{i}")), format!("file {i}")).unwrap();
        }
        let s = small(&path, 4);
        let handles: Vec<(u64, u64)> = (0..1000)
            .map(|i| {
                let node = found(&s, ROOT, &format!("f{i}"));
                (node, open(&s, node, 0))
            })
            .collect();
        for _ in 0..3 {
            for (i, &(node, fh)) in handles.iter().enumerate() {
                assert_eq!(
                    read(&s, node, fh, 0, 32),
                    (0, format!("file {i}").into_bytes()),
                    "f{i}"
                );
            }
            let st = s.state.lock().unwrap();
            assert!(
                st.fds.open.len() <= 4 + REQUEST_FDS as usize,
                "{} held",
                st.fds.open.len()
            );
        }
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A file the guest unlinks by one of its names while it has it open needs no pin while
    /// another name remains: with no room for one, its handles are reached by that name, and
    /// so is a new one. Its last name unlinked with no room, they are stale once let go.
    #[test]
    fn a_file_unlinked_by_one_name_is_served_by_another() {
        let (path, _) = dir();
        std::fs::write(path.join("a"), "linked").unwrap();
        std::fs::hard_link(path.join("a"), path.join("b")).unwrap();
        std::fs::write(path.join("p"), "p").unwrap();
        crowded(&path, 20);
        let s = small(&path, 1);
        let p = found(&s, ROOT, "p");
        let _fp = open(&s, p, 0);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("p"))).0, 0);
        let a = found(&s, ROOT, "a");
        let fa = open(&s, a, 0);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("a"))).0, 0);
        assert_eq!(found(&s, ROOT, "b"), a, "one node, known by its other name now");
        crowd(&s, 20);
        assert_eq!(read(&s, a, fa, 0, 16), (0, b"linked".to_vec()));
        let again = open(&s, a, 0);
        assert_eq!(read(&s, a, again, 0, 16), (0, b"linked".to_vec()));
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("b"))).0, 0);
        crowd(&s, 20);
        assert_eq!(read(&s, a, fa, 0, 16).0, -ESTALE);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A file whose last name the guest unlinks while it has it open, with no room to pin
    /// it, is stale once its handles' descriptors are let go, even where its path then leads
    /// to a file of its number: a host's filesystem gives a freed number again (ext4 at
    /// once), and that file is not the one the handles opened. One opened after reads it.
    #[test]
    fn a_file_unlinked_unpinned_is_not_found_again_by_its_number() {
        use std::os::unix::fs::MetadataExt as _;
        let (path, _) = dir();
        std::fs::write(path.join("p"), "p").unwrap();
        std::fs::write(path.join("a"), "gone").unwrap();
        crowded(&path, 20);
        let s = small(&path, 1);
        let p = found(&s, ROOT, "p");
        let _fp = open(&s, p, 0);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("p"))).0, 0);
        let a = found(&s, ROOT, "a");
        let fa = open(&s, a, 0);
        assert_eq!(answer(&s, &req(op::UNLINK, ROOT, 0, &name("a"))).0, 0);
        // Another file at its name, given its number as a host may give it.
        std::fs::write(path.join("a"), "another").unwrap();
        let m = std::fs::metadata(path.join("a")).unwrap();
        s.state.lock().unwrap().nodes.get_mut(&a).unwrap().key = (m.dev(), m.ino());
        crowd(&s, 20);
        assert_eq!(read(&s, a, fa, 0, 16).0, -ESTALE);
        let fresh = open(&s, a, 0);
        assert_eq!(read(&s, a, fresh, 0, 16), (0, b"another".to_vec()));
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn ownership_records_read_and_write_back() {
        let owner = Owner::parse(br#"{"UID":1234,"GID":5678,"mode":600}"#).unwrap();
        assert_eq!((owner.uid, owner.gid, owner.mode), (1234, 5678, Some(0o600)));
        assert_eq!(owner.encode(), r#"{"UID":1234,"GID":5678,"mode":600}"#);
        assert!(Owner::parse(b"{}").is_none());
    }
}
