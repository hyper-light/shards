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
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::sync::{Mutex, PoisonError};

use super::host::{self, linux};

const ROOT: u64 = 1;
/// How long the guest may trust what it was told of a name or its attributes: virtiofsd's
/// `cache=auto` second, as the host may change the directory meanwhile.
const VALID_S: u64 = 1;
/// The most the guest writes or reads in one request: 256 pages.
pub const MAX_PAGES: u16 = 256;
pub const MAX_WRITE: u32 = MAX_PAGES as u32 * 4096;
// `mode_t` is u16 on macOS and u32 on Linux: its values as the guest's u32.
#[allow(clippy::unnecessary_cast)]
const S_IFMT: u32 = libc::S_IFMT as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFDIR: u32 = libc::S_IFDIR as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFREG: u32 = libc::S_IFREG as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFLNK: u32 = libc::S_IFLNK as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFCHR: u32 = libc::S_IFCHR as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFBLK: u32 = libc::S_IFBLK as u32;
#[allow(clippy::unnecessary_cast)]
const S_ISUID: u32 = libc::S_ISUID as u32;
#[allow(clippy::unnecessary_cast)]
const S_ISGID: u32 = libc::S_ISGID as u32;

/// A stat's mode as the guest's u32.
#[allow(clippy::unnecessary_cast)]
fn mode_of(st: &libc::stat) -> u32 {
    st.st_mode as u32
}

const OWNER_XATTR: &str = "com.docker.grpcfuse.ownership";

mod op {
    pub const LOOKUP: u32 = 1;
    pub const FORGET: u32 = 2;
    pub const GETATTR: u32 = 3;
    pub const SETATTR: u32 = 4;
    pub const READLINK: u32 = 5;
    pub const SYMLINK: u32 = 6;
    pub const MKNOD: u32 = 8;
    pub const MKDIR: u32 = 9;
    pub const UNLINK: u32 = 10;
    pub const RMDIR: u32 = 11;
    pub const RENAME: u32 = 12;
    pub const LINK: u32 = 13;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const WRITE: u32 = 16;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FSYNC: u32 = 20;
    pub const SETXATTR: u32 = 21;
    pub const GETXATTR: u32 = 22;
    pub const LISTXATTR: u32 = 23;
    pub const REMOVEXATTR: u32 = 24;
    pub const FLUSH: u32 = 25;
    pub const INIT: u32 = 26;
    pub const OPENDIR: u32 = 27;
    pub const READDIR: u32 = 28;
    pub const RELEASEDIR: u32 = 29;
    pub const FSYNCDIR: u32 = 30;
    pub const ACCESS: u32 = 34;
    pub const CREATE: u32 = 35;
    pub const INTERRUPT: u32 = 36;
    pub const DESTROY: u32 = 38;
    pub const BATCH_FORGET: u32 = 42;
    pub const FALLOCATE: u32 = 43;
    pub const READDIRPLUS: u32 = 44;
    pub const RENAME2: u32 = 45;
    pub const LSEEK: u32 = 46;
    pub const SYNCFS: u32 = 50;
}

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
}

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
    lookups: u64,
    key: (u64, u64),
    /// The node of the volume it is in, in a join share ([`Server::joined`]); none
    /// ([`NO_VOLUME`]) in a share of one directory, and the join share's root's own.
    volume: u64,
}

/// No volume's: a node of a share of one directory, or a join share's root. Node IDs start
/// at [`ROOT`], so none is 0.
const NO_VOLUME: u64 = 0;

/// A join share's volume (D119): a directory a container joining the microVM's network is
/// given, reached from the share's empty root by its name, read-only or not, or of its one
/// name, held for as long as it is served.
#[derive(Debug)]
struct Volume {
    fd: OwnedFd,
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
    flags: Option<libc::c_int>,
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
    Fd(OwnedFd),
    Dir(DirStream),
}

#[derive(Debug)]
struct State {
    /// The shared directory, held for as long as the server: a join share's an empty one,
    /// its volumes reached from it by name.
    root: OwnedFd,
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
    pins: HashMap<u64, (OwnedFd, libc::c_int)>,
    /// Nodes whose last name the guest removed with no room to pin them, and the first handle
    /// opened after: those before it are stale once their descriptors are let go, as the
    /// number the node's path might lead to is free to be another file's.
    unreached: HashMap<u64, u64>,
    /// The descriptors the server may hold between requests ([`Limits::budget`]): its pins,
    /// and in the room they leave, its directories' and handles' descriptors.
    budget: u64,
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

/// A guest's file owner, as kept on the host (Docker Desktop's attribute).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Owner {
    uid: u32,
    gid: u32,
    /// The permission bits it was given, where the attribute holds them.
    mode: Option<u32>,
}

impl Owner {
    /// `{"UID":U,"GID":G,"mode":M}`, M the mode's octal digits read as decimal.
    fn parse(bytes: &[u8]) -> Option<Owner> {
        let text = std::str::from_utf8(bytes).ok()?;
        let field = |key: &str| -> Option<u64> {
            let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
            let rest = text.get(at..)?.trim_start().strip_prefix(':')?.trim_start();
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        };
        Some(Owner {
            uid: u32::try_from(field("UID")?).ok()?,
            gid: u32::try_from(field("GID")?).ok()?,
            mode: field("mode").and_then(|m| u32::from_str_radix(&m.to_string(), 8).ok()),
        })
    }

    fn encode(&self) -> String {
        match self.mode {
            Some(m) => format!(
                "{{\"UID\":{},\"GID\":{},\"mode\":{:o}}}",
                self.uid,
                self.gid,
                m & 0o7777
            ),
            None => format!("{{\"UID\":{},\"GID\":{}}}", self.uid, self.gid),
        }
    }
}

fn cstring(s: &str) -> Result<CString, Errno> {
    CString::new(s).map_err(|_| EINVAL)
}

/// `fd`'s attribute `name`, or none.
fn fget_xattr(fd: RawFd, name: &CStr) -> Result<Option<Vec<u8>>, Errno> {
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

fn fset_xattr(fd: RawFd, name: &CStr, value: &[u8], flags: u32) -> Result<(), Errno> {
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

fn stat_fd(fd: RawFd) -> Result<libc::stat, Errno> {
    // SAFETY: an all-zero stat is a valid out-parameter for fstat(2).
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat(2) of a descriptor we hold.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(last());
    }
    Ok(st)
}

fn stat_at(dir: RawFd, name: &CStr) -> Result<libc::stat, Errno> {
    // SAFETY: as stat_fd.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstatat(2) of a NUL-terminated name in a directory we hold, not following it.
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(last());
    }
    Ok(st)
}

#[cfg(test)]
thread_local! {
    /// The names this thread's server has opened, for a test of what it never opens.
    static OPENED: std::cell::RefCell<Vec<CString>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int, mode: libc::c_uint) -> Result<OwnedFd, Errno> {
    #[cfg(test)]
    OPENED.with(|o| o.borrow_mut().push(name.to_owned()));
    // SAFETY: openat(2) of a NUL-terminated name in a directory we hold.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(last());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `name` in `dir` opened for a guest's OPEN or CREATE, with `flags` (`host::open_flags`'),
/// if it is a regular file. A guest kernel opens FIFOs, sockets and device nodes itself and
/// sends no FUSE request to; one that asks would have this process wait on a FIFO's other
/// end for good, or open a host device, so they are refused (EBADF), as QEMU's virtiofsd
/// refuses them (`lo_inode_open`, CVE-2020-35517; audit V02). The name is looked at first,
/// so no special file is opened, then opened without waiting and looked at again, for one
/// put there meanwhile.
fn open_regular(
    dir: RawFd,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> Result<(OwnedFd, libc::stat), Errno> {
    let exclusive = flags & (libc::O_CREAT | libc::O_EXCL) == libc::O_CREAT | libc::O_EXCL;
    match stat_at(dir, name) {
        // An exclusive create of a name that exists fails at the open, opening nothing.
        Ok(st) if !exclusive && mode_of(&st) & S_IFMT != S_IFREG => return Err(EBADF),
        Ok(_) => {}
        Err(ENOENT) if flags & libc::O_CREAT != 0 => {}
        Err(e) => return Err(e),
    }
    let fd = open_at(dir, name, flags | libc::O_NONBLOCK | libc::O_NOCTTY, mode)?;
    let st = stat_fd(fd.as_raw_fd())?;
    if mode_of(&st) & S_IFMT != S_IFREG {
        return Err(EBADF);
    }
    // The status flags the guest asked for, without O_NONBLOCK: F_SETFL takes those of
    // `flags` and ignores its access mode and creation flags (POSIX fcntl(2)).
    // SAFETY: fcntl(2) of a descriptor we hold.
    if flags & libc::O_NONBLOCK == 0 && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags) } != 0 {
        return Err(last());
    }
    Ok((fd, st))
}

/// Changes the mode of `name` in `dir` without following it. fchmodat(2) without
/// AT_SYMLINK_NOFOLLOW follows a final symlink: a guest's link to a host file outside the
/// share would have that file's mode changed (audit V01). A symlink's own mode is not
/// changed, as Linux changes none (EOPNOTSUPP: fs/attr.c `notify_change` since 6.6, and
/// glibc's and musl's fchmodat with AT_SYMLINK_NOFOLLOW before it).
fn chmod_at(dir: RawFd, name: &CStr, mode: u32) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        // Linux's fchmodat(2) takes no AT_SYMLINK_NOFOLLOW before fchmodat2 (6.6). The
        // name is opened as a path, never followed, and the file changed through its
        // /proc/self/fd link, which reaches that file whatever is at the name by then
        // (musl's src/stat/fchmodat.c, virtiofsd's setattr).
        let fd = open_at(dir, name, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
        if mode_of(&stat_fd(fd.as_raw_fd())?) & S_IFMT == S_IFLNK {
            return Err(EOPNOTSUPP);
        }
        let link = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(|_| EINVAL)?;
        // SAFETY: fchmodat(2) of a NUL-terminated path.
        if unsafe { libc::fchmodat(libc::AT_FDCWD, link.as_ptr(), mode as libc::mode_t, 0) } != 0 {
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
                mode as libc::mode_t,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(last());
        }
        Ok(())
    }
}

fn key(st: &libc::stat) -> (u64, u64) {
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    (st.st_dev as u64, st.st_ino as u64)
}

fn is_dir(st: &libc::stat) -> bool {
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
    /// struct fuse_attr.
    fn attr(&mut self, st: &libc::stat, owner: Owner) -> &mut Self {
        #[allow(clippy::useless_conversion, clippy::unnecessary_cast, clippy::cast_sign_loss)]
        let (size, blocks, at, mt, ct) = (
            st.st_size as u64,
            st.st_blocks as u64,
            (st.st_atime as u64, st.st_atime_nsec as u32),
            (st.st_mtime as u64, st.st_mtime_nsec as u32),
            (st.st_ctime as u64, st.st_ctime_nsec as u32),
        );
        let host_mode = mode_of(st);
        let perm = owner.mode.unwrap_or(host_mode & 0o7777);
        let mode = (host_mode & S_IFMT) | (perm & 0o7777);
        #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
        let (nlink, rdev, blksize) = (st.st_nlink as u32, st.st_rdev as u64, st.st_blksize as u32);
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
            .u32(host::encode_dev(rdev))
            .u32(blksize)
            .u32(0)
    }
    /// struct fuse_entry_out.
    fn entry(&mut self, nodeid: u64, st: &libc::stat, owner: Owner) -> &mut Self {
        self.u64(nodeid)
            .u64(0)
            .u64(VALID_S)
            .u64(VALID_S)
            .u32(0)
            .u32(0)
            .attr(st, owner)
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
    pub fn new(root: OwnedFd, read_only: bool, only: Option<CString>) -> Result<Server, String> {
        let limits = Limits::now().ok_or("this process's limit on descriptors is unknown")?;
        Server::with_budget(root, read_only, only, limits.budget(1))
    }

    /// [`Server::new`], holding at most `budget` descriptors between requests.
    pub fn with_budget(
        root: OwnedFd,
        read_only: bool,
        only: Option<CString>,
        budget: u64,
    ) -> Result<Server, String> {
        let st = stat_fd(root.as_raw_fd()).map_err(|e| format!("the shared directory: errno {e}"))?;
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
            }),
        })
    }

    /// A join share's server (D119): its root the empty directory `root` holds, nothing
    /// made there, and each volume [`add`](Self::add)ed reached from it by its name.
    pub fn joined(root: OwnedFd, budget: u64) -> Result<Server, String> {
        let server = Server::with_budget(root, false, None, budget)?;
        server.state.lock().unwrap_or_else(PoisonError::into_inner).joined = true;
        Ok(server)
    }

    /// Adds volume `name` to a join share: the directory `dir` holds, read-only or not, or
    /// of its one name `only`, a root of its own under the share's.
    pub fn add(
        &self,
        name: CString,
        dir: OwnedFd,
        read_only: bool,
        only: Option<CString>,
    ) -> Result<(), String> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
            return Err(format!("a volume named {name:?}"));
        }
        let st = stat_fd(dir.as_raw_fd()).map_err(|e| format!("volume {name:?}: errno {e}"))?;
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

    /// Answers request `req`, the bytes of its readable descriptors: the reply's bytes,
    /// none for a request that takes none (FORGET, BATCH_FORGET, INTERRUPT).
    pub fn handle(&self, req: &[u8]) -> Option<Vec<u8>> {
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
            return None;
        };
        let body = req
            .get(40..usize::try_from(len).unwrap_or(0).min(req.len()))
            .unwrap_or_default();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let caller = (uid, gid);
        let answer = match opcode {
            op::FORGET => {
                let n = Args(body).u64().unwrap_or(0);
                forget(&mut state, nodeid, n);
                return None;
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
                return None;
            }
            op::INTERRUPT => return None,
            _ => match self.confined(&state, opcode, nodeid, body) {
                Ok(()) => self.dispatch(&mut state, opcode, nodeid, caller, body),
                Err(e) => Err(e),
            },
        };
        // Between requests, the descriptors held fit the room the pins leave.
        let pins = u64::try_from(state.pins.len()).unwrap_or(u64::MAX);
        let room = state.budget.saturating_sub(pins);
        state.fds.trim(room);
        let mut out = Vec::new();
        let (error, payload) = match answer {
            Ok(r) => (0i32, r.0),
            Err(e) => (-e, Vec::new()),
        };
        let total = u32::try_from(16 + payload.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&error.to_le_bytes());
        out.extend_from_slice(&unique.to_le_bytes());
        out.extend_from_slice(&payload);
        Some(out)
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
    ) -> Result<Reply, Errno> {
        let mut a = Args(body);
        let mut r = Reply::default();
        match opcode {
            op::INIT => {
                // A session begins: nothing the guest knew before it holds.
                reset(s);
                let major = a.u32()?;
                let minor = a.u32()?;
                let readahead = a.u32()?;
                let flags = a.u32()?;
                if major != 7 {
                    // The kernel retries with ours.
                    r.u32(7).u32(45);
                    return Ok(r);
                }
                let want = init::ASYNC_READ
                    | init::ATOMIC_O_TRUNC
                    | init::BIG_WRITES
                    | init::DO_READDIRPLUS
                    | init::READDIRPLUS_AUTO
                    | init::PARALLEL_DIROPS
                    | init::MAX_PAGES
                    | init::CACHE_SYMLINKS;
                r.u32(7)
                    .u32(minor.min(45))
                    .u32(readahead)
                    .u32(flags & want)
                    .u16(16)
                    .u16(12)
                    .u32(MAX_WRITE)
                    .u32(1)
                    .u16(MAX_PAGES)
                    .u16(0)
                    .u32(0)
                    .u32(0)
                    .u16(0)
                    .bytes(&[0u8; 22]);
            }
            op::DESTROY => reset(s),
            op::LOOKUP => {
                let name = a.name()?;
                let (id, st, owner) = lookup(s, nodeid, &name)?;
                r.entry(id, &st, owner);
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
                r.u64(VALID_S).u32(0).u32(0).attr(&st, owner);
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
                r.u64(VALID_S).u32(0).u32(0).attr(&st, owner);
            }
            op::READLINK => {
                let (dir, name) = at(s, nodeid)?;
                let mut buf = vec![0u8; 4096];
                // SAFETY: readlinkat(2) into a buffer of its length.
                let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    return Err(last());
                }
                buf.truncate(usize::try_from(n).unwrap_or(0));
                r.bytes(&buf);
            }
            op::SYMLINK => {
                self.writable(s, nodeid)?;
                let name = a.name()?;
                let target = a.cstr()?;
                let dir = dir_fd(s, nodeid)?;
                // SAFETY: symlinkat(2) of NUL-terminated strings in a directory we hold.
                if unsafe { libc::symlinkat(target.as_ptr(), dir, name.as_ptr()) } != 0 {
                    return Err(last());
                }
                self.made(s, nodeid, &name, caller, &mut r)?;
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
                if kind == S_IFREG || kind == 0 {
                    drop(open_at(
                        dir,
                        &name,
                        libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
                        mode & 0o7777,
                    )?);
                } else {
                    // SAFETY: mknodat(2) of a NUL-terminated name in a directory we hold.
                    if unsafe {
                        libc::mknodat(dir, name.as_ptr(), mode as libc::mode_t, host::decode_dev(rdev))
                    } != 0
                    {
                        return Err(last());
                    }
                }
                self.made(s, nodeid, &name, caller, &mut r)?;
            }
            op::MKDIR => {
                self.writable(s, nodeid)?;
                let mode = a.u32()?;
                let _umask = a.u32()?;
                let name = a.name()?;
                let dir = dir_fd(s, nodeid)?;
                // SAFETY: mkdirat(2) of a NUL-terminated name in a directory we hold.
                if unsafe { libc::mkdirat(dir, name.as_ptr(), (mode & 0o7777) as libc::mode_t) } != 0 {
                    return Err(last());
                }
                self.made(s, nodeid, &name, caller, &mut r)?;
            }
            op::UNLINK | op::RMDIR => {
                self.writable(s, nodeid)?;
                let name = a.name()?;
                let dir = dir_fd(s, nodeid)?;
                let removal = pin_before_removal(s, volume_of(s, nodeid), dir, &name);
                let flags = if opcode == op::RMDIR {
                    libc::AT_REMOVEDIR
                } else {
                    0
                };
                // SAFETY: unlinkat(2) of a NUL-terminated name in a directory we hold.
                let failed = (unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0).then(last);
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
                rename(s, nodeid, &old, newdir, &new, flags)?;
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
                // SAFETY: linkat(2) of NUL-terminated names in directories we hold.
                if unsafe { libc::linkat(odir, oname.as_ptr(), dir, name.as_ptr(), 0) } != 0 {
                    return Err(last());
                }
                let (id, st, owner) = lookup(s, nodeid, &name)?;
                r.entry(id, &st, owner);
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
                let flags = host::open_flags(flags) | libc::O_CREAT;
                let (fd, st) = open_regular(dir, &name, flags, mode & 0o7777)?;
                let made = self.made(s, nodeid, &name, caller, &mut r)?;
                // Made with a mode that refuses its own handle (a read-only file opened to be
                // filled, as tar, cp and git make one): none could be opened for it again, so
                // this descriptor is its pin.
                let held = if refuses(mode_of(&st), flags)
                    && s.nodes.get(&made).is_some_and(|n| n.key == key(&st))
                    && !s.pins.contains_key(&made)
                    && room_for_pin(s)
                {
                    s.pins.insert(made, (fd, flags & libc::O_ACCMODE));
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
                let mut buf = vec![0u8; size as usize];
                let n = pread(fd, &mut buf, offset)?;
                buf.truncate(n);
                r.bytes(&buf);
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
                let dir = dir_fd(s, top)?;
                // SAFETY: an all-zero statvfs is a valid out-parameter.
                let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: fstatvfs(2) of a descriptor we hold.
                if unsafe { libc::fstatvfs(dir, &mut v) } != 0 {
                    return Err(last());
                }
                #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
                r.u64(v.f_blocks as u64)
                    .u64(v.f_bfree as u64)
                    .u64(v.f_bavail as u64)
                    .u64(v.f_files as u64)
                    .u64(v.f_ffree as u64)
                    .u32(v.f_bsize as u32)
                    .u32(255)
                    .u32(v.f_frsize as u32)
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
                    Some((pin, _)) => pin.as_raw_fd(),
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
                    if plus {
                        if bytes == b"." || bytes == b".." {
                            r.bytes(&[0u8; 128]);
                        } else {
                            match lookup(s, nodeid, &entry.name) {
                                Ok((id, st, owner)) => {
                                    r.entry(id, &st, owner);
                                }
                                // Gone since the directory was read: an entry the guest
                                // looks up, and does not find.
                                Err(_) => {
                                    r.bytes(&[0u8; 128]);
                                }
                            }
                        }
                    }
                    r.u64(entry.ino)
                        .u64(next)
                        .u32(u32::try_from(bytes.len()).unwrap_or(0))
                        .u32(entry.kind)
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
                let fd = node_fd(s, nodeid)?;
                let value = fget_xattr(fd.raw(), &name)?.ok_or(ENODATA)?;
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
                let fd = node_fd_to_change(s, nodeid)?;
                fset_xattr(fd.raw(), &name, value, flags)?;
            }
            op::REMOVEXATTR => {
                self.writable(s, nodeid)?;
                let name = a.cstr()?;
                if hidden(&name) {
                    return Err(ENODATA);
                }
                let fd = node_fd_to_change(s, nodeid)?;
                // SAFETY: fremovexattr(2) of a NUL-terminated name.
                #[cfg(target_os = "macos")]
                let rc = unsafe { libc::fremovexattr(fd.raw(), name.as_ptr(), 0) };
                // SAFETY: as above.
                #[cfg(target_os = "linux")]
                let rc = unsafe { libc::fremovexattr(fd.raw(), name.as_ptr()) };
                if rc != 0 {
                    return Err(last());
                }
            }
            op::LSEEK => {
                let fh = a.u64()?;
                let offset = a.u64()?;
                let whence = host::whence(a.u32()?).ok_or(EINVAL)?;
                let fd = file_fd(s, fh)?;
                // SAFETY: lseek(2) of a descriptor we hold.
                let at = unsafe { libc::lseek(fd, offset as libc::off_t, whence) };
                if at < 0 {
                    return Err(last());
                }
                r.u64(at as u64);
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
        Ok(r)
    }

    /// The node made as `name` in `parent` by `caller`: its owner kept where it is not
    /// root, or where `parent` passes its group on (setgid); then looked up.
    fn made(
        &self,
        s: &mut State,
        parent: u64,
        name: &CStr,
        caller: (u32, u32),
        r: &mut Reply,
    ) -> Result<u64, Errno> {
        let parent_st = node_stat(s, parent)?;
        let parent_owner = node_owner(s, parent, None)?;
        let gid = if mode_of(&parent_st) & S_ISGID != 0 {
            parent_owner.gid
        } else {
            caller.1
        };
        if caller.0 != 0 || gid != 0 {
            let dir = dir_fd(s, parent)?;
            if let Ok(fd) = open_meta(dir, name, None) {
                let owner = Owner {
                    uid: caller.0,
                    gid,
                    mode: None,
                };
                let _ = fset_xattr(fd.raw(), &owner_xattr()?, owner.encode().as_bytes(), 0);
            }
        }
        let (id, st, owner) = lookup(s, parent, name)?;
        r.entry(id, &st, owner);
        Ok(id)
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
            let truncated = match valid & fattr::FH != 0 && is_file(s, fh) {
                true => {
                    let fd = file_fd(s, fh)?;
                    // SAFETY: ftruncate(2) of a descriptor we hold.
                    unsafe { libc::ftruncate(fd, size as libc::off_t) }
                }
                false => {
                    let fd = open_at(
                        dir,
                        &name,
                        libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                        0,
                    )?;
                    // SAFETY: as above.
                    unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) }
                }
            };
            if truncated != 0 {
                return Err(last());
            }
        }
        if valid & (fattr::ATIME | fattr::MTIME) != 0 {
            let time = |set: bool, now: bool, s_: u64, ns: u32| libc::timespec {
                tv_sec: s_ as _,
                tv_nsec: if now {
                    libc::UTIME_NOW
                } else if set {
                    libc::c_long::from(ns as i32)
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
                    libc::utimensat(dir, name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
                },
            };
            if rc != 0 {
                return Err(last());
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn chmod(
        &self,
        s: &mut State,
        nodeid: u64,
        own: Option<RawFd>,
        dir: RawFd,
        name: &CStr,
        mode: u32,
        owner: &mut Owner,
        had: bool,
    ) -> Result<(), Errno> {
        // Its open handles a mode refuses keep a descriptor taken before it does (audit V09).
        pin_before_chmod(s, nodeid, dir, name, mode);
        match own {
            Some(fd) => {
                // SAFETY: fchmod(2) of a descriptor we hold.
                if unsafe { libc::fchmod(fd, mode as libc::mode_t) } != 0 {
                    return Err(last());
                }
            }
            None => chmod_at(dir, name, mode)?,
        }
        // The kept mode follows, where the attribute is kept.
        if had {
            owner.mode = Some(mode);
            let fd = node_fd_to_change(s, nodeid)?;
            fset_xattr(fd.raw(), &owner_xattr()?, owner.encode().as_bytes(), 0)?;
        }
        Ok(())
    }
}

/// The bytes of a request's header that an answer of an error alone needs: its length,
/// opcode and `unique`.
pub const HEADER_BYTES: usize = 16;

/// The answer to `req` where no directory is shared yet: ENODEV, or none for a request
/// that takes no reply.
pub fn unattached(req: &[u8]) -> Option<Vec<u8>> {
    failed(req, 19)
}

/// The answer to a request longer than any FUSE request, of which `req` holds the first
/// [`HEADER_BYTES`]: EINVAL, or none for one that takes no reply.
pub fn too_long(req: &[u8]) -> Option<Vec<u8>> {
    failed(req, EINVAL)
}

/// `errno` as the answer to `req`, from the first [`HEADER_BYTES`] of its header.
fn failed(req: &[u8], errno: Errno) -> Option<Vec<u8>> {
    let opcode = u32::from_le_bytes(req.get(4..8)?.try_into().ok()?);
    if matches!(opcode, op::FORGET | op::BATCH_FORGET | op::INTERRUPT) {
        return None;
    }
    let unique = req.get(8..HEADER_BYTES)?;
    let mut out = Vec::with_capacity(HEADER_BYTES);
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&(-errno).to_le_bytes());
    out.extend_from_slice(unique);
    Some(out)
}

/// A descriptor of a node, for its attributes: a directory's own, or one opened for it.
enum NodeFd {
    Borrowed(RawFd),
    Owned(OwnedFd),
}

impl NodeFd {
    fn raw(&self) -> RawFd {
        match self {
            NodeFd::Borrowed(fd) => *fd,
            NodeFd::Owned(fd) => fd.as_raw_fd(),
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
fn open_meta(dir: RawFd, name: &CStr, known: Option<u32>) -> Result<NodeFd, Errno> {
    let kind = match known {
        Some(kind) => kind,
        None => mode_of(&stat_at(dir, name)?) & S_IFMT,
    };
    if matches!(kind, S_IFCHR | S_IFBLK) {
        return Err(ENODATA);
    }
    #[cfg(target_os = "macos")]
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_SYMLINK;
    #[cfg(target_os = "linux")]
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY;
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
    let parent = node.parent;
    let dir = dir_fd(s, parent)?;
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    open_meta(dir, &node.name, known)
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
fn node_owner(s: &mut State, nodeid: u64, st: Option<&libc::stat>) -> Result<Owner, Errno> {
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
fn dir_fd(s: &mut State, nodeid: u64) -> Result<RawFd, Errno> {
    if nodeid == ROOT {
        return Ok(s.root.as_raw_fd());
    }
    if let Some(v) = s.volumes.get(&nodeid) {
        return Ok(v.fd.as_raw_fd());
    }
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    if !node.dir {
        return Err(ENOTDIR);
    }
    let want = node.key;
    if let Some(Held::Fd(fd)) = s.fds.get(Slot::Node(nodeid)) {
        return Ok(fd.as_raw_fd());
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
            break s.root.as_raw_fd();
        }
        if let Some(v) = s.volumes.get(&at) {
            break v.fd.as_raw_fd();
        }
        if let Some(Held::Fd(fd)) = s.fds.get(Slot::Node(at)) {
            break fd.as_raw_fd();
        }
        if names.len() > s.nodes.len() {
            return Err(ESTALE);
        }
    };
    names.reverse();
    let fd = open_beneath(from, &names)?;
    let st = stat_fd(fd.as_raw_fd())?;
    if !is_dir(&st) || key(&st) != want {
        return Err(ESTALE);
    }
    let raw = fd.as_raw_fd();
    s.fds.insert(Slot::Node(nodeid), Held::Fd(fd));
    Ok(raw)
}

/// The directory `names` lead to under `dir`, a component after another, none followed
/// if a symlink: on Linux in one openat2(2) refusing symlinks and any way out of `dir`
/// (RESOLVE_NO_SYMLINKS, RESOLVE_BENEATH; Linux 5.6), and a component at a time with
/// O_NOFOLLOW where the kernel has no openat2, or the path is too long for one call, and on
/// macOS. A path that no longer leads to a directory is stale.
fn open_beneath(dir: RawFd, names: &[CString]) -> Result<OwnedFd, Errno> {
    #[cfg(target_os = "linux")]
    if let Some(opened) = openat2_beneath(dir, names) {
        return opened.map_err(stale);
    }
    let mut held: Option<OwnedFd> = None;
    for name in names {
        let at = held.as_ref().map_or(dir, std::os::fd::AsRawFd::as_raw_fd);
        let next = open_at(at, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW, 0);
        held = Some(next.map_err(stale)?);
    }
    held.ok_or(EINVAL)
}

/// [`open_beneath`] in one openat2(2); none where this kernel has no openat2 (ENOSYS, or a
/// seccomp filter's EPERM) or the path is longer than one call takes.
#[cfg(target_os = "linux")]
fn openat2_beneath(dir: RawFd, names: &[CString]) -> Option<Result<OwnedFd, Errno>> {
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
        flags: u64::try_from(libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC).ok()?,
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
    if let Ok(fd) = RawFd::try_from(fd)
        && fd >= 0
    {
        // SAFETY: a fresh descriptor nothing else owns.
        return Some(Ok(unsafe { OwnedFd::from_raw_fd(fd) }));
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
fn at(s: &mut State, nodeid: u64) -> Result<(RawFd, CString), Errno> {
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    if node.dir {
        return Ok((dir_fd(s, nodeid)?, cstring(".")?));
    }
    let (parent, name) = (node.parent, node.name.clone());
    Ok((dir_fd(s, parent)?, name))
}

fn node_stat(s: &mut State, nodeid: u64) -> Result<libc::stat, Errno> {
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    if node.dir {
        return stat_fd(dir_fd(s, nodeid)?);
    }
    let parent = node.parent;
    let dir = dir_fd(s, parent)?;
    let node = s.nodes.get(&nodeid).ok_or(ENOENT)?;
    stat_at(dir, &node.name)
}

/// `name` in directory node `parent`: its node, counted once more and known by this name
/// now, its attributes and its owner. A directory is not opened until it is used.
fn lookup(s: &mut State, parent: u64, name: &CStr) -> Result<(u64, libc::stat, Owner), Errno> {
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
                    (n.parent, n.name, n.dir) = (parent, name.to_owned(), is_dir(&st));
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
fn once(flags: libc::c_int) -> libc::c_int {
    flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC)
}

/// Whether a descriptor opened with access mode `access` serves `flags`' access.
fn allows(access: libc::c_int, flags: libc::c_int) -> bool {
    match flags & libc::O_ACCMODE {
        libc::O_RDONLY => access != libc::O_WRONLY,
        libc::O_WRONLY => access != libc::O_RDONLY,
        _ => access == libc::O_RDWR,
    }
}

/// A handle of the guest's on `node`, opened with host `flags` (a file's) or none (a
/// directory's), its descriptor `held` where the open took one.
fn add_handle(s: &mut State, node: u64, flags: Option<libc::c_int>, held: Option<Held>) -> u64 {
    let fh = s.next_handle;
    s.next_handle += 1;
    let opened = s.opened.entry(node).or_default();
    opened.handles += 1;
    if let Some(flags) = flags {
        let access = flags & libc::O_ACCMODE;
        if access != libc::O_WRONLY {
            opened.reading += 1;
        }
        if access != libc::O_RDONLY {
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
        let access = flags & libc::O_ACCMODE;
        if access != libc::O_WRONLY {
            opened.reading = opened.reading.saturating_sub(1);
        }
        if access != libc::O_RDONLY {
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
fn file_fd(s: &mut State, fh: u64) -> Result<RawFd, Errno> {
    let h = s.handles.get(&fh).ok_or(EBADF)?;
    let (node, flags) = (h.node, h.flags.ok_or(EBADF)?);
    if let Some((pin, _)) = s.pins.get(&node) {
        return Ok(pin.as_raw_fd());
    }
    if let Some(Held::Fd(fd)) = s.fds.get(Slot::Handle(fh)) {
        return Ok(fd.as_raw_fd());
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
    let raw = fd.as_raw_fd();
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
            Some((pin, _)) => pin.as_raw_fd(),
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
fn pin_before_removal(s: &mut State, volume: u64, dir: RawFd, name: &CStr) -> Removal {
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
        (_, false) => libc::O_RDONLY,
        (false, true) => libc::O_WRONLY,
        (true, true) => libc::O_RDWR,
    };
    // A file the removal leaves another name of needs no pin where there is no room: that
    // name reaches it, and the guest's lookup of it makes it the node's path.
    if !room_for_pin(s) && !is_dir(&st) && st.st_nlink > 1 {
        return Removal::Nothing;
    }
    let fd = if !room_for_pin(s) {
        None
    } else if is_dir(&st) {
        open_at(
            dir,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .ok()
    } else {
        open_regular(dir, name, access | libc::O_NOFOLLOW, 0)
            .ok()
            .map(|(fd, _)| fd)
    };
    match fd.filter(|fd| stat_fd(fd.as_raw_fd()).is_ok_and(|now| key(&now) == key(&st))) {
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
fn pin_before_chmod(s: &mut State, node: u64, dir: RawFd, name: &CStr, mode: u32) {
    if s.pins.contains_key(&node) || !room_for_pin(s) {
        return;
    }
    let Some(opened) = s.opened.get(&node) else {
        return;
    };
    let access = match (opened.reading > 0, opened.writing > 0) {
        (_, false) => libc::O_RDONLY,
        (false, true) => libc::O_WRONLY,
        (true, true) => libc::O_RDWR,
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
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .ok(),
        Ok(_) => open_regular(dir, name, access | libc::O_NOFOLLOW, 0)
            .ok()
            .map(|(fd, _)| fd),
        Err(_) => None,
    };
    if let Some(fd) = fd.filter(|fd| stat_fd(fd.as_raw_fd()).is_ok_and(|now| key(&now) == want)) {
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
fn refuses(mode: u32, flags: libc::c_int) -> bool {
    let access = flags & libc::O_ACCMODE;
    let reads = access != libc::O_WRONLY;
    let writes = access != libc::O_RDONLY;
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
struct DirStream(OwnedFd);

#[cfg(target_os = "linux")]
impl DirStream {
    fn open(dir: RawFd) -> Result<DirStream, Errno> {
        open_at(dir, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0).map(DirStream)
    }

    /// Gives `each` the entries from `offset` on, with the offset that follows each, until
    /// it says to stop or the directory ends; `want` bytes of them are read at a time.
    fn read(
        &mut self,
        offset: u64,
        want: usize,
        mut each: impl FnMut(&Entry, u64) -> bool,
    ) -> Result<(), Errno> {
        let fd = self.0.as_raw_fd();
        // SAFETY: lseek(2) of a descriptor we hold.
        if unsafe { libc::lseek(fd, offset as libc::off_t, libc::SEEK_SET) } < 0 {
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
    fn open(dir: RawFd) -> Result<DirStream, Errno> {
        use std::os::fd::IntoRawFd as _;
        let fd = open_at(dir, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        // SAFETY: fdopendir(3) of a descriptor we own; the stream owns it once it exists.
        let stream = std::ptr::NonNull::new(unsafe { libc::fdopendir(fd.as_raw_fd()) }).ok_or_else(last)?;
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
    }
    Ok(())
}

/// `renameat2(2)` of `old` in `od` to `new` in `nd` with Linux's `flags`, as the host makes
/// it: `renameat`, macOS's `renameatx_np`, or Linux's own system call.
fn renamed(od: RawFd, old: &CStr, nd: RawFd, new: &CStr, flags: u32) -> Result<(), Errno> {
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
                libc::syscall(libc::SYS_renameat2, od, old.as_ptr(), nd, new.as_ptr(), flags) as libc::c_int
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

/// The names of `fd`'s attributes the guest may see: those in a Linux namespace, but for
/// the server's own.
fn list_xattr(fd: RawFd) -> Result<Vec<u8>, Errno> {
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
        let linux = [&b"user."[..], b"trusted.", b"security.", b"system."]
            .iter()
            .any(|p| name.starts_with(p));
        let ours =
            name == OWNER_XATTR.as_bytes() || name.strip_prefix(b"user.") == Some(OWNER_XATTR.as_bytes());
        if linux && !ours {
            out.extend_from_slice(name);
            out.push(0);
        }
    }
    Ok(out)
}

fn pread(fd: RawFd, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
    let mut done = 0;
    while done < buf.len() {
        let rest = buf.get_mut(done..).ok_or(EIO)?;
        // SAFETY: pread(2) into the rest of our buffer.
        let n = unsafe {
            libc::pread(
                fd,
                rest.as_mut_ptr().cast(),
                rest.len(),
                (offset + done as u64) as libc::off_t,
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

fn pwrite(fd: RawFd, buf: &[u8], offset: u64) -> Result<usize, Errno> {
    let mut done = 0;
    while done < buf.len() {
        let rest = buf.get(done..).ok_or(EIO)?;
        // SAFETY: pwrite(2) of the rest of the guest's data.
        let n = unsafe {
            libc::pwrite(
                fd,
                rest.as_ptr().cast(),
                rest.len(),
                (offset + done as u64) as libc::off_t,
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
fn durable(fd: RawFd) -> Result<(), Errno> {
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
fn fallocate(fd: RawFd, mode: u32, offset: u64, length: u64) -> Result<(), Errno> {
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
                mode as libc::c_int,
                offset as libc::off_t,
                length as libc::off_t,
            )
        } != 0
        {
            return Err(last());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let size = stat_fd(fd)?.st_size as u64;
        if end > size {
            let mut store = libc::fstore_t {
                fst_flags: libc::F_ALLOCATEALL,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: (end - size) as libc::off_t,
                fst_bytesalloc: 0,
            };
            // SAFETY: fcntl(2) F_PREALLOCATE with a store of ours.
            if unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut store) } != 0 {
                return Err(last());
            }
            if mode & linux::FALLOC_FL_KEEP_SIZE == 0 {
                // SAFETY: ftruncate(2) of a descriptor we hold.
                if unsafe { libc::ftruncate(fd, end as libc::off_t) } != 0 {
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
        let mode = std::fs::metadata(path.join("ro")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o444);
        assert!(s.state.lock().unwrap().pins.is_empty(), "let go with its handle");
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
    fn ownership_is_docker_desktops() {
        let owner = Owner::parse(br#"{"UID":1234,"GID":5678,"mode":600}"#).unwrap();
        assert_eq!((owner.uid, owner.gid, owner.mode), (1234, 5678, Some(0o600)));
        assert_eq!(owner.encode(), r#"{"UID":1234,"GID":5678,"mode":600}"#);
        assert!(Owner::parse(b"{}").is_none());
    }
}
