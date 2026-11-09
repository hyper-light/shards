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
/// The guest's page, which FUSE counts `max_pages` in (fs/fuse/inode.c l. 1342-1344, Linux
/// 1590cf03): 4 KiB, x86-64's least (Intel SDM Vol. 3A §4.5) and arm64's as shards' kernel
/// is built (resources/kernel/firecracker-aarch64-6.18.config l. 437, CONFIG_ARM64_4K_PAGES).
pub const GUEST_PAGE: u32 = 4096;
pub const MAX_WRITE: u32 = MAX_PAGES as u32 * GUEST_PAGE;

/// The most a share's tables hold, the bytes of its guest's removed files among them: what
/// one control frame carries to or from its server, the 31 bits of its length word below
/// the bit that marks it (`CONTROL`). The state that keeps them is bounded besides by its
/// machine's RAM (`snapshot::most_state`, D176), which a save checks as a restore does.
pub const MAX_TABLES: usize = (super::CONTROL - 1) as usize;

/// The bytes of a request's header that an answer of an error alone needs: its length,
/// opcode and `unique`.
pub const HEADER_BYTES: usize = 16;
/// The bytes of a request's whole header (struct fuse_in_header), after which its
/// arguments come.
pub const IN_HEADER_BYTES: usize = 40;
/// The bytes of a reply's header (struct fuse_out_header): its length, error and `unique`.
pub const OUT_HEADER_BYTES: usize = 16;
/// The bytes of a WRITE's arguments before its data (struct fuse_write_in,
/// include/uapi/linux/fuse.h l. 839-847): its handle, offset, size, write flags, lock owner,
/// flags and padding.
pub const WRITE_IN_BYTES: usize = 40;
/// A SETXATTR's arguments (struct fuse_setxattr_in, l. 868-873), at their longest, with
/// its name of at most XATTR_NAME_MAX bytes and its NUL and its value of at most
/// XATTR_SIZE_MAX (include/uapi/linux/limits.h l. 15-16): the longest request of a guest's
/// but a WRITE's.
pub const SETXATTR_MOST: usize = 16 + 255 + 1 + 65536;

/// A Linux errno, as a reply carries it negated.
pub(super) type Errno = i32;
/// Linux's EIO, ENODEV and EINVAL (include/uapi/asm-generic/errno-base.h): the guest's numbers, whatever the host's,
/// which the device answers itself with where it has no server's answer; and EPERM, a
/// share's answer on Windows to a mapping asked by a process that holds no end of its
/// stream.
pub(super) const EIO: Errno = 5;
pub(super) const ENODEV: Errno = 19;
pub(super) const EINVAL: Errno = 22;
#[cfg_attr(unix, allow(dead_code))]
pub(super) const EPERM: Errno = 1;

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
