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

use std::collections::HashMap;
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

/// A node the guest knows: a directory by its own descriptor; anything else by its
/// parent's and its name. `lookups` is the kernel's count of it (FORGET's).
#[derive(Debug)]
enum Kind {
    Dir(OwnedFd),
    Entry { parent: u64, name: CString },
}

#[derive(Debug)]
struct Node {
    kind: Kind,
    lookups: u64,
    key: (u64, u64),
}

/// An open file, or a directory's entries as they were when it was opened.
#[derive(Debug)]
enum Handle {
    File(OwnedFd),
    Dir(Vec<(CString, u64, u32)>),
}

#[derive(Debug)]
struct State {
    nodes: HashMap<u64, Node>,
    by_key: HashMap<(u64, u64), u64>,
    next_node: u64,
    handles: HashMap<u64, Handle>,
    next_handle: u64,
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

const EIO: Errno = 5;
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

fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int, mode: libc::c_uint) -> Result<OwnedFd, Errno> {
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
fn open_regular(dir: RawFd, name: &CStr, flags: libc::c_int, mode: libc::c_uint) -> Result<OwnedFd, Errno> {
    let exclusive = flags & (libc::O_CREAT | libc::O_EXCL) == libc::O_CREAT | libc::O_EXCL;
    match stat_at(dir, name) {
        // An exclusive create of a name that exists fails at the open, opening nothing.
        Ok(st) if !exclusive && mode_of(&st) & S_IFMT != S_IFREG => return Err(EBADF),
        Ok(_) => {}
        Err(ENOENT) if flags & libc::O_CREAT != 0 => {}
        Err(e) => return Err(e),
    }
    let fd = open_at(dir, name, flags | libc::O_NONBLOCK | libc::O_NOCTTY, mode)?;
    if mode_of(&stat_fd(fd.as_raw_fd())?) & S_IFMT != S_IFREG {
        return Err(EBADF);
    }
    // The status flags the guest asked for, without O_NONBLOCK: F_SETFL takes those of
    // `flags` and ignores its access mode and creation flags (POSIX fcntl(2)).
    // SAFETY: fcntl(2) of a descriptor we hold.
    if flags & libc::O_NONBLOCK == 0 && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags) } != 0 {
        return Err(last());
    }
    Ok(fd)
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

impl Server {
    /// The server of the directory `root` holds, read-only or not, or of its one name
    /// `only`.
    pub fn new(root: OwnedFd, read_only: bool, only: Option<CString>) -> Result<Server, String> {
        let st = stat_fd(root.as_raw_fd()).map_err(|e| format!("the shared directory: errno {e}"))?;
        if !is_dir(&st) {
            return Err("the shared directory is not a directory".into());
        }
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT,
            Node {
                kind: Kind::Dir(root),
                lookups: 1,
                key: key(&st),
            },
        );
        let mut by_key = HashMap::new();
        by_key.insert(key(&st), ROOT);
        Ok(Server {
            read_only,
            only,
            state: Mutex::new(State {
                nodes,
                by_key,
                next_node: ROOT + 1,
                handles: HashMap::new(),
                next_handle: 1,
            }),
        })
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
            _ => match self.confined(opcode, nodeid, body) {
                Ok(()) => self.dispatch(&mut state, opcode, nodeid, caller, body),
                Err(e) => Err(e),
            },
        };
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

    /// Whether a file bound alone lets the request through: at its directory, only the
    /// file's lookup and what reads the directory itself; no name moves in or out, as none
    /// does of a file bind mount (EBUSY).
    fn confined(&self, opcode: u32, nodeid: u64, body: &[u8]) -> Result<(), Errno> {
        let Some(only) = &self.only else {
            return Ok(());
        };
        match opcode {
            op::RENAME | op::RENAME2 | op::LINK => Err(EBUSY),
            _ if nodeid != ROOT => Ok(()),
            op::LOOKUP if Args(body).name()?.as_c_str() == only.as_c_str() => Ok(()),
            op::LOOKUP => Err(ENOENT),
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
            | op::SYNCFS => Ok(()),
            op::UNLINK if Args(body).name()?.as_c_str() == only.as_c_str() => Err(EBUSY),
            _ => Err(EACCES),
        }
    }

    fn writable(&self) -> Result<(), Errno> {
        if self.read_only { Err(EROFS) } else { Ok(()) }
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
            op::DESTROY => {}
            op::LOOKUP => {
                let name = a.name()?;
                let (id, st, owner) = lookup(s, nodeid, &name)?;
                r.entry(id, &st, owner);
            }
            op::GETATTR => {
                let flags = a.u32()?;
                let _ = a.u32()?;
                let fh = a.u64()?;
                let st = if flags & 1 != 0 {
                    match s.handles.get(&fh) {
                        Some(Handle::File(fd)) => stat_fd(fd.as_raw_fd())?,
                        _ => node_stat(s, nodeid)?,
                    }
                } else {
                    node_stat(s, nodeid)?
                };
                let owner = node_owner(s, nodeid)?;
                r.u64(VALID_S).u32(0).u32(0).attr(&st, owner);
            }
            op::SETATTR => {
                self.writable()?;
                self.setattr(s, nodeid, &mut a)?;
                let st = node_stat(s, nodeid)?;
                let owner = node_owner(s, nodeid)?;
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
                self.writable()?;
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
                self.writable()?;
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
                self.writable()?;
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
                self.writable()?;
                let name = a.name()?;
                let dir = dir_fd(s, nodeid)?;
                let flags = if opcode == op::RMDIR {
                    libc::AT_REMOVEDIR
                } else {
                    0
                };
                // SAFETY: unlinkat(2) of a NUL-terminated name in a directory we hold.
                if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0 {
                    return Err(last());
                }
            }
            op::RENAME | op::RENAME2 => {
                self.writable()?;
                let newdir = a.u64()?;
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
                self.writable()?;
                let old = a.u64()?;
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
                    self.writable()?;
                }
                let (dir, name) = at(s, nodeid)?;
                if matches!(s.nodes.get(&nodeid).map(|n| &n.kind), Some(Kind::Dir(_))) {
                    return Err(EISDIR);
                }
                let fd = open_regular(dir, &name, host::open_flags(flags & !linux::O_CREAT), 0)?;
                let fh = add_handle(s, Handle::File(fd));
                r.u64(fh).u32(0).u32(0);
            }
            op::CREATE => {
                self.writable()?;
                let flags = a.u32()?;
                let mode = a.u32()?;
                let _umask = a.u32()?;
                let _ = a.u32()?;
                let name = a.name()?;
                let dir = dir_fd(s, nodeid)?;
                let fd = open_regular(dir, &name, host::open_flags(flags) | libc::O_CREAT, mode & 0o7777)?;
                self.made(s, nodeid, &name, caller, &mut r)?;
                let fh = add_handle(s, Handle::File(fd));
                r.u64(fh).u32(0).u32(0);
            }
            op::READ => {
                let fh = a.u64()?;
                let offset = a.u64()?;
                let size = a.u32()?.min(MAX_WRITE);
                let Some(Handle::File(fd)) = s.handles.get(&fh) else {
                    return Err(EBADF);
                };
                let mut buf = vec![0u8; size as usize];
                let n = pread(fd.as_raw_fd(), &mut buf, offset)?;
                buf.truncate(n);
                r.bytes(&buf);
            }
            op::WRITE => {
                self.writable()?;
                let fh = a.u64()?;
                let offset = a.u64()?;
                let size = a.u32()?;
                let _flags = a.u32()?;
                let _owner = a.u64()?;
                let _ = a.u32()?;
                let _ = a.u32()?;
                let data = a.rest().get(..size as usize).ok_or(EINVAL)?;
                let Some(Handle::File(fd)) = s.handles.get(&fh) else {
                    return Err(EBADF);
                };
                let n = pwrite(fd.as_raw_fd(), data, offset)?;
                r.u32(u32::try_from(n).unwrap_or(0)).u32(0);
            }
            op::STATFS => {
                let dir = dir_fd(s, ROOT)?;
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
                s.handles.remove(&fh);
            }
            op::FLUSH => {}
            op::FSYNC | op::FSYNCDIR => {
                let fh = a.u64()?;
                let fd = match s.handles.get(&fh) {
                    Some(Handle::File(fd)) => fd.as_raw_fd(),
                    _ => dir_fd(s, nodeid)?,
                };
                durable(fd)?;
            }
            op::SYNCFS => {}
            op::OPENDIR => {
                let dir = dir_fd(s, nodeid).map_err(|_| ENOTDIR)?;
                let mut entries = read_dir(dir)?;
                if let (Some(only), ROOT) = (&self.only, nodeid) {
                    entries.retain(|(name, _, _)| matches!(name.to_bytes(), b"." | b"..") || name == only);
                }
                let fh = add_handle(s, Handle::Dir(entries));
                r.u64(fh).u32(0).u32(0);
            }
            op::READDIR | op::READDIRPLUS => {
                let fh = a.u64()?;
                let offset = a.u64()?;
                let size = a.u32()? as usize;
                let Some(Handle::Dir(entries)) = s.handles.get(&fh) else {
                    return Err(EBADF);
                };
                let entries: Vec<(CString, u64, u32)> = entries
                    .iter()
                    .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                    .cloned()
                    .collect();
                let plus = opcode == op::READDIRPLUS;
                for (i, (name, ino, kind)) in entries.into_iter().enumerate() {
                    let bytes = name.as_bytes();
                    let base = if plus { 152 } else { 24 };
                    let len = (base + bytes.len() + 7) & !7;
                    if r.0.len() + len > size {
                        break;
                    }
                    let next = offset.saturating_add(i as u64 + 1);
                    if plus {
                        if bytes == b"." || bytes == b".." {
                            r.bytes(&[0u8; 128]);
                        } else {
                            match lookup(s, nodeid, &name) {
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
                    r.u64(ino)
                        .u64(next)
                        .u32(u32::try_from(bytes.len()).unwrap_or(0))
                        .u32(kind)
                        .bytes(bytes);
                    let pad = len - base - bytes.len();
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
                let fd = node_fd(s, nodeid)?;
                let names = list_xattr(fd.raw())?;
                if size == 0 {
                    r.u32(u32::try_from(names.len()).unwrap_or(u32::MAX)).u32(0);
                } else if names.len() > size as usize {
                    return Err(ERANGE);
                } else {
                    r.bytes(&names);
                }
            }
            op::SETXATTR => {
                self.writable()?;
                let size = a.u32()?;
                let flags = a.u32()?;
                let name = a.cstr()?;
                let value = a.rest().get(..size as usize).ok_or(EINVAL)?;
                if hidden(&name) {
                    return Err(EOPNOTSUPP);
                }
                let fd = node_fd(s, nodeid)?;
                fset_xattr(fd.raw(), &name, value, flags)?;
            }
            op::REMOVEXATTR => {
                self.writable()?;
                let name = a.cstr()?;
                if hidden(&name) {
                    return Err(ENODATA);
                }
                let fd = node_fd(s, nodeid)?;
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
                let Some(Handle::File(fd)) = s.handles.get(&fh) else {
                    return Err(EBADF);
                };
                // SAFETY: lseek(2) of a descriptor we hold.
                let at = unsafe { libc::lseek(fd.as_raw_fd(), offset as libc::off_t, whence) };
                if at < 0 {
                    return Err(last());
                }
                r.u64(at as u64);
            }
            op::FALLOCATE => {
                self.writable()?;
                let fh = a.u64()?;
                let offset = a.u64()?;
                let length = a.u64()?;
                let mode = a.u32()?;
                let Some(Handle::File(fd)) = s.handles.get(&fh) else {
                    return Err(EBADF);
                };
                fallocate(fd.as_raw_fd(), mode, offset, length)?;
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
    ) -> Result<(), Errno> {
        let parent_st = node_stat(s, parent)?;
        let parent_owner = node_owner(s, parent)?;
        let gid = if mode_of(&parent_st) & S_ISGID != 0 {
            parent_owner.gid
        } else {
            caller.1
        };
        if caller.0 != 0 || gid != 0 {
            let dir = dir_fd(s, parent)?;
            if let Ok(fd) = open_meta(dir, name) {
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
        let own = match s.nodes.get(&nodeid).map(|n| &n.kind) {
            Some(Kind::Dir(fd)) => Some(fd.as_raw_fd()),
            _ => None,
        };
        let mut owner = node_owner(s, nodeid)?;
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
            let fd = node_fd(s, nodeid)?;
            fset_xattr(fd.raw(), &owner_xattr()?, owner.encode().as_bytes(), 0)?;
        }
        if valid & fattr::SIZE != 0 {
            let truncated = match (valid & fattr::FH != 0).then(|| s.handles.get(&fh)).flatten() {
                Some(Handle::File(fd)) => {
                    // SAFETY: ftruncate(2) of a descriptor we hold.
                    unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) }
                }
                _ => {
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
            let fd = node_fd(s, nodeid)?;
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

/// `name` in `dir`, opened only to read and set its attributes: not followed, and not
/// waiting on a FIFO's writer.
fn open_meta(dir: RawFd, name: &CStr) -> Result<NodeFd, Errno> {
    #[cfg(target_os = "macos")]
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_SYMLINK;
    #[cfg(target_os = "linux")]
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY;
    open_at(dir, name, flags, 0).map(NodeFd::Owned)
}

fn node_fd(s: &State, nodeid: u64) -> Result<NodeFd, Errno> {
    match &s.nodes.get(&nodeid).ok_or(ENOENT)?.kind {
        Kind::Dir(fd) => Ok(NodeFd::Borrowed(fd.as_raw_fd())),
        Kind::Entry { parent, name } => open_meta(dir_fd(s, *parent)?, name),
    }
}

fn node_has_owner(s: &State, nodeid: u64) -> bool {
    node_fd(s, nodeid)
        .ok()
        .and_then(|fd| fget_xattr(fd.raw(), &owner_xattr().ok()?).ok().flatten())
        .is_some()
}

/// The guest's owner of a node: its attribute's, or root.
fn node_owner(s: &State, nodeid: u64) -> Result<Owner, Errno> {
    let Ok(fd) = node_fd(s, nodeid) else {
        return Ok(Owner::default());
    };
    Ok(fget_xattr(fd.raw(), &owner_xattr()?)
        .ok()
        .flatten()
        .and_then(|v| Owner::parse(&v))
        .unwrap_or_default())
}

fn dir_fd(s: &State, nodeid: u64) -> Result<RawFd, Errno> {
    match &s.nodes.get(&nodeid).ok_or(ENOENT)?.kind {
        Kind::Dir(fd) => Ok(fd.as_raw_fd()),
        Kind::Entry { .. } => Err(ENOTDIR),
    }
}

/// A node's parent's descriptor and its name; the root's own and `.`.
fn at(s: &State, nodeid: u64) -> Result<(RawFd, CString), Errno> {
    match &s.nodes.get(&nodeid).ok_or(ENOENT)?.kind {
        Kind::Dir(fd) => Ok((fd.as_raw_fd(), cstring(".")?)),
        Kind::Entry { parent, name } => Ok((dir_fd(s, *parent)?, name.clone())),
    }
}

fn node_stat(s: &State, nodeid: u64) -> Result<libc::stat, Errno> {
    match &s.nodes.get(&nodeid).ok_or(ENOENT)?.kind {
        Kind::Dir(fd) => stat_fd(fd.as_raw_fd()),
        Kind::Entry { parent, name } => stat_at(dir_fd(s, *parent)?, name),
    }
}

/// `name` in directory node `parent`: its node, counted once more, its attributes and its
/// owner.
fn lookup(s: &mut State, parent: u64, name: &CStr) -> Result<(u64, libc::stat, Owner), Errno> {
    let dir = dir_fd(s, parent)?;
    let st = stat_at(dir, name)?;
    let k = key(&st);
    let id = match s.by_key.get(&k).copied() {
        Some(id) if s.nodes.contains_key(&id) => {
            if let Some(n) = s.nodes.get_mut(&id) {
                n.lookups = n.lookups.saturating_add(1);
                // A name it is known by now, where it is not a directory.
                if let Kind::Entry { parent: p, name: n_ } = &mut n.kind {
                    *p = parent;
                    *n_ = name.to_owned();
                }
            }
            id
        }
        _ => {
            let kind = if is_dir(&st) {
                Kind::Dir(open_at(
                    dir,
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    0,
                )?)
            } else {
                Kind::Entry {
                    parent,
                    name: name.to_owned(),
                }
            };
            let id = s.next_node;
            s.next_node += 1;
            s.nodes.insert(
                id,
                Node {
                    kind,
                    lookups: 1,
                    key: k,
                },
            );
            s.by_key.insert(k, id);
            id
        }
    };
    let owner = node_owner(s, id)?;
    Ok((id, st, owner))
}

fn forget(s: &mut State, nodeid: u64, n: u64) {
    if nodeid == ROOT {
        return;
    }
    let gone = match s.nodes.get_mut(&nodeid) {
        Some(node) => {
            node.lookups = node.lookups.saturating_sub(n);
            node.lookups == 0
        }
        None => false,
    };
    if gone
        && let Some(node) = s.nodes.remove(&nodeid)
        && s.by_key.get(&node.key) == Some(&nodeid)
    {
        s.by_key.remove(&node.key);
    }
}

fn add_handle(s: &mut State, h: Handle) -> u64 {
    let fh = s.next_handle;
    s.next_handle += 1;
    s.handles.insert(fh, h);
    fh
}

/// The entries of directory `dir`, with their inode numbers and Linux dirent types.
fn read_dir(dir: RawFd) -> Result<Vec<(CString, u64, u32)>, Errno> {
    // SAFETY: dup(2) of a descriptor we hold; fdopendir takes the copy.
    let copy = unsafe { libc::dup(dir) };
    if copy < 0 {
        return Err(last());
    }
    // SAFETY: fdopendir(3) of a fresh descriptor, which the stream then owns.
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        let e = last();
        // SAFETY: closing the copy fdopendir did not take.
        unsafe { libc::close(copy) };
        return Err(e);
    }
    // SAFETY: rewinddir(3) of the open stream: dup shares the offset.
    unsafe { libc::rewinddir(stream) };
    let mut out = Vec::new();
    loop {
        // SAFETY: readdir(3) of our open stream; the entry is copied before the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: readdir returned a valid entry, its name NUL-terminated.
        let (name, ino, kind) = unsafe {
            let e = &*entry;
            (
                CStr::from_ptr(e.d_name.as_ptr()).to_owned(),
                e.d_ino as u64,
                u32::from(e.d_type),
            )
        };
        out.push((name, ino, kind));
    }
    // SAFETY: closedir(3) of our stream, which closes the copy.
    unsafe { libc::closedir(stream) };
    Ok(out)
}

fn rename(s: &mut State, olddir: u64, old: &CStr, newdir: u64, new: &CStr, flags: u32) -> Result<(), Errno> {
    let (od, nd) = (dir_fd(s, olddir)?, dir_fd(s, newdir)?);
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
    // The names nodes are known by follow.
    let exchange = flags & linux::RENAME_EXCHANGE != 0;
    for node in s.nodes.values_mut() {
        if let Kind::Entry { parent, name } = &mut node.kind {
            if *parent == olddir && name.as_c_str() == old {
                (*parent, *name) = (newdir, new.to_owned());
            } else if exchange && *parent == newdir && name.as_c_str() == new {
                (*parent, *name) = (olddir, old.to_owned());
            }
        }
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

    fn dir() -> (std::path::PathBuf, Server) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("shards-fs-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let fd = std::fs::File::open(&p).unwrap();
        (p, Server::new(fd.into(), false, None).unwrap())
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
        let outside = path.with_extension("outside");
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
            let Some(Handle::File(fd)) = state.handles.get(&fh) else {
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

    #[test]
    fn ownership_is_docker_desktops() {
        let owner = Owner::parse(br#"{"UID":1234,"GID":5678,"mode":600}"#).unwrap();
        assert_eq!((owner.uid, owner.gid, owner.mode), (1234, 5678, Some(0o600)));
        assert_eq!(owner.encode(), r#"{"UID":1234,"GID":5678,"mode":600}"#);
        assert!(Owner::parse(b"{}").is_none());
    }
}
