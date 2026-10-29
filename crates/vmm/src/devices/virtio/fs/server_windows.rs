//! The share server's calls of a Windows host's file system (D197): NTFS kept as WSL keeps
//! a Linux tree, through `ntfs`, each relative to a handle of the shared tree and following
//! no reparse point. Its flags, modes and errno are Linux's own (host.rs). A guest's owner,
//! group and mode are WSL's `$LX` attributes, which the server's owner attribute is read and
//! written as; its other extended attributes NTFS EAs of names of their own.

use std::os::windows::io::{AsHandle as _, BorrowedHandle};

use windows_sys::Wdk::Storage::FileSystem::{FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF};
use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_WRITE_ATTRIBUTES, FILE_WRITE_EA,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::super::ntfs;
use super::*;

/// The flags a node is opened with for its metadata, never following it.
pub(super) const META_FLAGS: host::c_int =
    host::O_RDONLY | host::O_NOFOLLOW | host::O_NONBLOCK | host::O_NOCTTY;

const EOPNOTSUPP: Errno = 95;
const ENXIO: Errno = 6;
const ERANGE: Errno = 34;

/// `raw` borrowed for a call: the server passes its host's calls only handles it holds open
/// for their length.
fn borrow<'a>(raw: host::Raw) -> BorrowedHandle<'a> {
    // SAFETY: a handle the server holds open across the call that borrows it.
    unsafe { BorrowedHandle::borrow_raw(raw) }
}

/// A guest's name as NTFS keeps it ([`ntfs::nt_name`]); `.`, its directory itself, none.
fn nt(name: &CStr) -> Result<Vec<u16>, Errno> {
    if name.to_bytes() == b"." {
        return Ok(Vec::new());
    }
    ntfs::nt_name(name.to_bytes())
}

/// `name` in `dir` opened as Linux's `flags` ask: its access mode, creation (exclusive,
/// or not), a directory where `O_DIRECTORY`, and truncation after the open, which keeps the
/// file's extended attributes, WSL's metadata among them, as FILE_OVERWRITE would not.
pub(super) fn open_at(
    dir: host::Raw,
    name: &CStr,
    flags: host::c_int,
    _mode: host::c_uint,
) -> Result<host::Owned, Errno> {
    #[cfg(test)]
    OPENED.with(|o| o.borrow_mut().push(name.to_owned()));
    let access = match flags & host::O_ACCMODE {
        host::O_WRONLY => FILE_GENERIC_WRITE,
        host::O_RDWR => FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        _ => FILE_GENERIC_READ,
    };
    let disposition = match (flags & host::O_CREAT != 0, flags & host::O_EXCL != 0) {
        (true, true) => FILE_CREATE,
        (true, false) => FILE_OPEN_IF,
        (false, _) => FILE_OPEN,
    };
    let options = if flags & host::O_DIRECTORY != 0 {
        FILE_DIRECTORY_FILE
    } else {
        0
    };
    let opened = ntfs::open_at(borrow(dir), &nt(name)?, access, disposition, options)?;
    if flags & host::O_TRUNC != 0 && flags & host::O_ACCMODE != host::O_RDONLY {
        ntfs::truncate(opened.as_handle(), 0)?;
    }
    Ok(opened)
}

/// `name` in `dir` opened for a guest's OPEN or CREATE, if it is a regular file (server_unix.rs's
/// rule): anything else refused (EBADF), a directory never opened as a file.
pub(super) fn open_regular(
    dir: host::Raw,
    name: &CStr,
    flags: host::c_int,
    mode: host::c_uint,
) -> Result<(host::Owned, Stat), Errno> {
    let opened = match open_at(dir, name, flags, mode) {
        Err(EISDIR) => return Err(EBADF),
        other => other?,
    };
    let st = ntfs::stat(opened.as_handle())?;
    if mode_of(&st) & S_IFMT != S_IFREG {
        return Err(EBADF);
    }
    Ok((opened, st))
}

/// `name` in `dir` opened for its metadata alone.
fn meta(dir: host::Raw, name: &CStr) -> Result<host::Owned, Errno> {
    ntfs::open_at(borrow(dir), &nt(name)?, 0, FILE_OPEN, 0)
}

pub(super) fn stat_fd(fd: host::Raw) -> Result<Stat, Errno> {
    ntfs::stat(borrow(fd))
}

pub(super) fn stat_at(dir: host::Raw, name: &CStr) -> Result<Stat, Errno> {
    ntfs::stat(meta(dir, name)?.as_handle())
}

/// What the symlink `name` in `dir` holds: an LX symlink's target.
pub(super) fn readlink_at(dir: host::Raw, name: &CStr) -> Result<Vec<u8>, Errno> {
    ntfs::readlink(meta(dir, name)?.as_handle())
}

/// A symlink `name` in `dir` holding `target`, as WSL makes one.
pub(super) fn symlink_at(target: &CStr, dir: host::Raw, name: &CStr) -> Result<(), Errno> {
    ntfs::symlink_at(borrow(dir), &nt(name)?, target.to_bytes()).map(drop)
}

/// A node `name` in `dir` of `mode`'s kind and the guest's device number `rdev`: a
/// regular file, or a FIFO, device or Unix socket as WSL makes one.
pub(super) fn mknod_at(dir: host::Raw, name: &CStr, mode: u32, rdev: u32) -> Result<(), Errno> {
    let kind = mode & S_IFMT;
    if kind == S_IFREG {
        return open_at(dir, name, host::O_CREAT | host::O_EXCL | host::O_WRONLY, 0).map(drop);
    }
    // Linux's new_decode_dev.
    let major = (rdev & 0xfff00) >> 8;
    let minor = (rdev & 0xff) | ((rdev >> 12) & 0xfff00);
    ntfs::special_at(borrow(dir), &nt(name)?, kind, (major, minor)).map(drop)
}

/// A directory `name` in `dir`, case-sensitive as WSL makes its own; its permissions the
/// owner's attribute's, as every mode here.
pub(super) fn mkdir_at(dir: host::Raw, name: &CStr, _perm: u32) -> Result<(), Errno> {
    ntfs::mkdir_at(borrow(dir), &nt(name)?).map(drop)
}

pub(super) fn unlink_at(dir: host::Raw, name: &CStr, rmdir: bool) -> Result<(), Errno> {
    ntfs::unlink_at(borrow(dir), &nt(name)?, rmdir)
}

pub(super) fn link_at(odir: host::Raw, oname: &CStr, dir: host::Raw, name: &CStr) -> Result<(), Errno> {
    ntfs::link_at(borrow(odir), &nt(oname)?, borrow(dir), &nt(name)?)
}

/// `h`'s file's mode set to its kind's and `perm` (`$LXMOD`); a symlink's own refused, as
/// Linux changes none (EOPNOTSUPP).
fn chmod_handle(h: BorrowedHandle<'_>, perm: u32) -> Result<(), Errno> {
    let kind = ntfs::stat(h)?.mode & S_IFMT;
    if kind == S_IFLNK {
        return Err(EOPNOTSUPP);
    }
    ntfs::set_lx(h, None, None, Some(kind | (perm & 0o7777)), None)
}

pub(super) fn chmod_at(dir: host::Raw, name: &CStr, mode: u32) -> Result<(), Errno> {
    let opened = ntfs::open_at(borrow(dir), &nt(name)?, FILE_WRITE_EA, FILE_OPEN, 0)?;
    chmod_handle(opened.as_handle(), mode)
}

pub(super) fn chmod_fd(fd: host::Raw, perm: u32) -> Result<(), Errno> {
    chmod_handle(borrow(fd), perm)
}

/// The user this process serves as: on Windows none of a guest's, its files root's where
/// WSL keeps no owner of theirs.
pub(super) fn served_by() -> u32 {
    0
}

/// Another handle of what `fd` is open to (DuplicateHandle, its access the same).
pub(super) fn dup_fd(fd: host::Raw) -> Result<host::Owned, Errno> {
    use std::os::windows::io::FromRawHandle as _;
    let mut copy = std::ptr::null_mut();
    // SAFETY: a handle the server holds, duplicated within this process into one to fill.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            fd,
            GetCurrentProcess(),
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(EIO);
    }
    // SAFETY: a handle DuplicateHandle just made, owned here alone.
    Ok(unsafe { host::Owned::from_raw_handle(copy) })
}

/// The volume `fd`'s file is on, as a guest's STATFS asks it: NTFS counts no inodes.
pub(super) fn fs_stats(fd: host::Raw) -> Result<FsStats, Errno> {
    let v = ntfs::volume(borrow(fd))?;
    Ok(FsStats {
        blocks: v.blocks,
        free: v.free,
        available: v.available,
        files: 0,
        files_free: 0,
        block_size: v.block_size,
        fragment_size: v.block_size,
    })
}

/// The EA a guest's extended attribute `name` is kept in: its bytes as hexadecimal after
/// `SH.`, as NTFS keeps an EA's name in upper case, ASCII, and at most 255 bytes long, where
/// a guest's names are any bytes, case kept (so at most 126 bytes of one, ERANGE past it).
fn ea_name(name: &[u8]) -> Result<Vec<u8>, Errno> {
    if name.len() > 126 {
        return Err(ERANGE);
    }
    let mut out = b"SH.".to_vec();
    for b in name {
        out.extend(format!("{b:02X}").bytes());
    }
    Ok(out)
}

/// A guest's attribute name for an EA [`ea_name`] made: none for any other EA.
fn ea_guest(ea: &[u8]) -> Option<Vec<u8>> {
    let hex = ea.strip_prefix(b"SH.")?;
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// The owner's attribute (server.rs `Owner`'s JSON) as WSL's metadata holds it: its owner,
/// group and mode (`$LXUID`, `$LXGID`, `$LXMOD`), a device's numbers (`$LXDEV`) where its
/// kind is one the host keeps no node of; none where WSL keeps none.
fn owner_of(fd: BorrowedHandle<'_>) -> Result<Option<Vec<u8>>, Errno> {
    let eas = ntfs::eas(fd)?;
    let word = |name: &[u8]| -> Option<u32> {
        let (_, v) = eas.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?;
        Some(u32::from_le_bytes(<[u8; 4]>::try_from(v.get(..4)?).ok()?))
    };
    let (uid, gid, mode) = (word(b"$LXUID"), word(b"$LXGID"), word(b"$LXMOD"));
    if uid.is_none() && gid.is_none() && mode.is_none() {
        return Ok(None);
    }
    let mut json = format!("{{\"UID\":{},\"GID\":{}", uid.unwrap_or(0), gid.unwrap_or(0));
    if let Some(m) = mode {
        json.push_str(&format!(",\"mode\":{:o}", m & 0o7777));
    }
    json.push('}');
    Ok(Some(json.into_bytes()))
}

pub(super) fn fget_xattr(fd: host::Raw, name: &CStr) -> Result<Option<Vec<u8>>, Errno> {
    if name.to_bytes() == OWNER_XATTR.as_bytes() {
        return owner_of(borrow(fd));
    }
    let ea = ea_name(name.to_bytes())?;
    let eas = ntfs::eas(borrow(fd))?;
    Ok(eas
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(&ea))
        .map(|(_, v)| v))
}

pub(super) fn fset_xattr(fd: host::Raw, name: &CStr, value: &[u8], flags: u32) -> Result<(), Errno> {
    let h = borrow(fd);
    if name.to_bytes() == OWNER_XATTR.as_bytes() {
        let owner = Owner::parse(value).ok_or(EINVAL)?;
        let kind = ntfs::stat(h)?.mode & S_IFMT;
        return ntfs::set_lx(
            h,
            Some(owner.uid),
            Some(owner.gid),
            owner.mode.map(|m| kind | m),
            None,
        );
    }
    let ea = ea_name(name.to_bytes())?;
    if flags & (linux::XATTR_CREATE | linux::XATTR_REPLACE) != 0 {
        let had = ntfs::eas(h)?.iter().any(|(n, _)| n.eq_ignore_ascii_case(&ea));
        if flags & linux::XATTR_CREATE != 0 && had {
            return Err(EEXIST);
        }
        if flags & linux::XATTR_REPLACE != 0 && !had {
            return Err(ENODATA);
        }
    }
    // NTFS keeps no EA of an empty value: such a one is removed.
    if value.is_empty() {
        return Err(EOPNOTSUPP);
    }
    ntfs::set_eas(h, &[(&ea, value)])
}

pub(super) fn fremove_xattr(fd: host::Raw, name: &CStr) -> Result<(), Errno> {
    let ea = ea_name(name.to_bytes())?;
    let h = borrow(fd);
    if !ntfs::eas(h)?.iter().any(|(n, _)| n.eq_ignore_ascii_case(&ea)) {
        return Err(ENODATA);
    }
    ntfs::set_eas(h, &[(&ea, &[])])
}

/// The guest's attribute names `fd`'s file has, each NUL-ended, as listxattr(2) gives them.
pub(super) fn list_xattr(fd: host::Raw) -> Result<Vec<u8>, Errno> {
    let mut out = Vec::new();
    for (ea, _) in ntfs::eas(borrow(fd))? {
        if let Some(name) = ea_guest(&ea) {
            out.extend(name);
            out.push(0);
        }
    }
    Ok(out)
}

/// The server's own name for its owner's attribute.
pub(super) fn owner_xattr() -> Result<CString, Errno> {
    cstring(OWNER_XATTR)
}

/// The host's name of a guest's attribute `name`: its own, as each is kept apart by
/// [`ea_name`]; the server's private names refused, as on Unix.
pub(super) fn host_xattr(name: &CStr) -> Result<CString, Errno> {
    if name.to_bytes().starts_with(PRIVATE_XATTR) {
        return Err(EOPNOTSUPP);
    }
    Ok(name.to_owned())
}

/// The prefix of the server's own attribute names, which a guest's may not take.
pub(super) const PRIVATE_XATTR: &[u8] = b"user.shards.";

/// `fd`'s offset from `whence` (Linux's): from its start or its end; its data from
/// `offset` on, or its hole, as a file every byte of which is data, as POSIX lets an
/// implementation keep one (SEEK_DATA and SEEK_HOLE, lseek(2)); no offset is kept between
/// reads, which name theirs.
pub(super) fn seek(fd: host::Raw, offset: u64, whence: host::c_int) -> Result<u64, Errno> {
    let size = ntfs::stat(borrow(fd))?.size;
    match u32::try_from(whence).map_err(|_| EINVAL)? {
        0 | 1 => Ok(offset),
        2 => size.checked_add(offset).ok_or(EINVAL),
        linux::SEEK_DATA if offset < size => Ok(offset),
        linux::SEEK_HOLE if offset < size => Ok(size),
        linux::SEEK_DATA | linux::SEEK_HOLE => Err(ENXIO),
        _ => Err(EINVAL),
    }
}

pub(super) fn truncate_fd(fd: host::Raw, size: u64) -> Result<(), Errno> {
    ntfs::truncate(borrow(fd), size)
}

/// Seconds and nanoseconds since the Unix epoch, now.
fn now() -> (u64, u32) {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (since.as_secs(), since.subsec_nanos())
}

pub(super) fn set_times(
    own: Option<host::Raw>,
    dir: host::Raw,
    name: &CStr,
    atime: Time,
    mtime: Time,
) -> Result<(), Errno> {
    let of = |t: Time| match t {
        Time::Now => Some(now()),
        Time::At(secs, nanos) => Some((secs, nanos)),
        Time::Kept => None,
    };
    match own {
        Some(fd) => ntfs::set_times(borrow(fd), of(atime), of(mtime)),
        None => {
            let opened = ntfs::open_at(borrow(dir), &nt(name)?, FILE_WRITE_ATTRIBUTES, FILE_OPEN, 0)?;
            ntfs::set_times(opened.as_handle(), of(atime), of(mtime))
        }
    }
}

/// `names` opened beneath `dir`, one component at a time, never following a reparse
/// point: one that is no directory along the way refused (ENOTDIR).
pub(super) fn open_beneath(dir: host::Raw, names: &[CString]) -> Result<host::Owned, Errno> {
    let (last, path) = names.split_last().ok_or(EINVAL)?;
    let mut at: Option<host::Owned> = None;
    for name in path {
        let from = at.as_ref().map_or(dir, host::AsRaw::raw);
        at = Some(ntfs::open_at(
            borrow(from),
            &nt(name)?,
            FILE_LIST_DIRECTORY,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )?);
    }
    let from = at.as_ref().map_or(dir, host::AsRaw::raw);
    ntfs::open_at(borrow(from), &nt(last)?, 0, FILE_OPEN, 0)
}

/// A directory's entries, read whole as it is opened and from its start again at offset
/// 0, served in turn: offsets the entries' own indexes, as the macOS stream counts them.
#[derive(Debug)]
pub(super) struct DirStream {
    dir: host::Owned,
    entries: Vec<Entry>,
}

impl DirStream {
    pub(super) fn open(dir: host::Raw) -> Result<DirStream, Errno> {
        let dir = ntfs::open_at(
            borrow(dir),
            &[],
            FILE_LIST_DIRECTORY,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )?;
        let entries = listed(dir.as_handle())?;
        Ok(DirStream { dir, entries })
    }

    /// Gives `each` the entries from `offset` on, with the offset that follows each, until
    /// it says to stop or the directory ends.
    pub(super) fn read(
        &mut self,
        offset: u64,
        _want: usize,
        mut each: impl FnMut(&Entry, u64) -> bool,
    ) -> Result<(), Errno> {
        if offset == 0 {
            self.entries = listed(self.dir.as_handle())?;
        }
        let from = usize::try_from(offset).unwrap_or(usize::MAX);
        for (i, entry) in self.entries.iter().enumerate().skip(from) {
            if !each(entry, u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1)) {
                break;
            }
        }
        Ok(())
    }
}

/// Every entry of `dir`, its kind as Linux's d_type.
fn listed(dir: BorrowedHandle<'_>) -> Result<Vec<Entry>, Errno> {
    let mut out = Vec::new();
    let mut restart = true;
    loop {
        let batch = ntfs::list(dir, restart)?;
        if batch.is_empty() {
            return Ok(out);
        }
        restart = false;
        for listed in batch {
            let kind = match listed.kind {
                S_IFDIR => 4,
                S_IFLNK => 10,
                S_IFIFO => u32::from(host::DT_FIFO),
                S_IFCHR => u32::from(host::DT_CHR),
                S_IFBLK => u32::from(host::DT_BLK),
                S_IFSOCK => u32::from(host::DT_SOCK),
                _ => u32::from(host::DT_REG),
            };
            out.push(Entry {
                name: CString::new(listed.name).map_err(|_| EINVAL)?,
                ino: listed.ino,
                kind,
            });
        }
    }
}

/// `old` in `od` renamed `new` in `nd` with Linux's `flags`: over nothing where
/// RENAME_NOREPLACE; an exchange NTFS cannot make refused (EINVAL), as by a file system
/// without it.
pub(super) fn renamed(od: host::Raw, old: &CStr, nd: host::Raw, new: &CStr, flags: u32) -> Result<(), Errno> {
    if flags & linux::RENAME_EXCHANGE != 0 {
        return Err(EINVAL);
    }
    let replace = flags & linux::RENAME_NOREPLACE == 0;
    ntfs::rename_at(borrow(od), &nt(old)?, borrow(nd), &nt(new)?, replace)
}

pub(super) fn pread(fd: host::Raw, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
    ntfs::read_at(borrow(fd), buf, offset)
}

/// The host's cache of what a read past its budgets brought in let go of: Windows lets a
/// process ask it of no file's ranges, and keeps it as its memory manager will.
pub(super) fn drop_cached(_fd: host::Raw, _offset: u64, _len: usize) {}

pub(super) fn pwrite(fd: host::Raw, buf: &[u8], offset: u64) -> Result<usize, Errno> {
    ntfs::write_at(borrow(fd), buf, offset)
}

pub(super) fn durable(fd: host::Raw) -> Result<(), Errno> {
    ntfs::sync(borrow(fd))
}

/// `length` bytes from `offset` made the file's: its size grown to them unless KEEP_SIZE,
/// NTFS allocating its clusters as they are written; any other mode refused (EOPNOTSUPP).
pub(super) fn fallocate(fd: host::Raw, mode: u32, offset: u64, length: u64) -> Result<(), Errno> {
    if mode & !linux::FALLOC_FL_KEEP_SIZE != 0 {
        return Err(EOPNOTSUPP);
    }
    if mode & linux::FALLOC_FL_KEEP_SIZE != 0 {
        return Ok(());
    }
    let end = offset.checked_add(length).ok_or(EINVAL)?;
    if end > ntfs::stat(borrow(fd))?.size {
        ntfs::truncate(borrow(fd), end)?;
    }
    Ok(())
}

/// A file in `dir` holding `bytes` that no name reaches (a restored server's ghost, D176):
/// made under a name of its own, written, and its name removed as it is held.
pub(super) fn ghost(dir: host::Raw, bytes: &[u8]) -> Result<host::Owned, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        ".shards-ghost-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    );
    let name = cstring(&name).map_err(|e| format!("a ghost's name: errno {e}"))?;
    let made = open_at(dir, &name, host::O_CREAT | host::O_EXCL | host::O_RDWR, 0)
        .map_err(|e| format!("making a ghost: errno {e}"))?;
    let mut at = 0usize;
    while at < bytes.len() {
        let n = ntfs::write_at(made.as_handle(), bytes.get(at..).unwrap_or_default(), at as u64)
            .map_err(|e| format!("writing a ghost: errno {e}"))?;
        if n == 0 {
            return Err("writing a ghost: none written".into());
        }
        at += n;
    }
    unlink_at(dir, &name, false).map_err(|e| format!("removing a ghost's name: errno {e}"))?;
    Ok(made)
}
