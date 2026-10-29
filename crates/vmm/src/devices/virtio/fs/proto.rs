//! What the device and every share's server agree on of FUSE's wire (include/uapi/linux/fuse.h), kept apart from the
//! share process's server (`server.rs`, Unix alone) so that the device builds on every host: its opcodes, its
//! headers' sizes, the most a request carries, and the answers of an error alone that the device gives itself where no
//! server can. On Windows a share's server is another's (D38; slates' daemon over a named pipe, PM M340), and the
//! device needs these and nothing else of `server.rs`.

/// The opcodes a request names (struct fuse_in_header's `opcode`).
pub mod op {
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

/// The most the guest writes or reads in one request: 256 pages.
pub const MAX_PAGES: u16 = shards_abi::SHARE_MAX_PAGES;
pub const MAX_WRITE: u32 = MAX_PAGES as u32 * 4096;

/// The most a share's tables hold, the bytes of its guest's removed files among them: what
/// a snapshot's state holds (D176).
pub const MAX_TABLES: usize = crate::snapshot::MAX_STATE as usize;

/// The bytes of a request's header that an answer of an error alone needs: its length,
/// opcode and `unique`.
pub const HEADER_BYTES: usize = 16;
/// The bytes of a request's whole header (struct fuse_in_header), after which its
/// arguments come.
pub const IN_HEADER_BYTES: usize = 40;
/// The bytes of a reply's header (struct fuse_out_header): its length, error and `unique`.
pub const OUT_HEADER_BYTES: usize = 16;

/// A Linux errno, as a reply carries it negated.
pub(super) type Errno = i32;
/// Linux's EIO, ENODEV and EINVAL (include/uapi/asm-generic/errno-base.h): the guest's numbers, whatever the host's,
/// which the device answers itself with where it has no server's answer; and EOPNOTSUPP
/// (asm-generic/errno.h), a share's answer on Windows to a mapping it has no window for.
pub(super) const EIO: Errno = 5;
pub(super) const ENODEV: Errno = 19;
pub(super) const EINVAL: Errno = 22;
#[cfg_attr(unix, allow(dead_code))]
pub(super) const EOPNOTSUPP: Errno = 95;

/// The answer to `req` where no directory is shared yet: ENODEV, or none for a request
/// that takes no reply.
pub fn unattached(req: &[u8]) -> Option<Vec<u8>> {
    failed(req, ENODEV)
}

/// The answer to a request longer than any FUSE request, of which `req` holds the first
/// [`HEADER_BYTES`]: EINVAL, or none for one that takes no reply.
pub fn too_long(req: &[u8]) -> Option<Vec<u8>> {
    failed(req, EINVAL)
}

/// The answer to a request whose bytes, or whose answer's, its device could not reach in
/// guest memory, of which `req` holds the first [`HEADER_BYTES`]: EIO, as a device whose
/// DMA failed, or none for a request that takes no reply.
pub fn unreachable(req: &[u8]) -> Option<Vec<u8>> {
    failed(req, EIO)
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
