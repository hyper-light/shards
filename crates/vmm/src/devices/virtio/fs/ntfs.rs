//! A guest's files on NTFS, kept as WSL keeps a Linux tree (D197, shards_ntfs): its calls,
//! each file's attributes as the server answers its guest.

use std::os::windows::io::BorrowedHandle;

pub use shards_ntfs::*;

use super::host;

/// [`shards_ntfs::stat`]: what `h`'s file is, as the server's guest is told.
pub fn stat(h: BorrowedHandle<'_>) -> Result<host::Stat, Errno> {
    shards_ntfs::stat(h).map(|s| host::Stat {
        dev: s.dev,
        ino: s.ino,
        mode: s.mode,
        nlink: s.nlink,
        uid: s.uid,
        size: s.size,
        blocks: s.blocks,
        blksize: s.blksize,
        rdev: s.rdev,
        atime: s.atime,
        mtime: s.mtime,
        ctime: s.ctime,
    })
}

/// Whether `theirs` is a client's end of the very pipe whose server end `ours` is: one name
/// (FileNameInfo; a share's pipe has a name of its own, `shards_ipc::byte_pair`), `ours` a
/// server's end and `theirs` none (GetNamedPipeInfo).
pub fn pipe_peers(ours: std::os::windows::io::RawHandle, theirs: std::os::windows::io::RawHandle) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{FileNameInfo, GetFileInformationByHandleEx};
    use windows_sys::Win32::System::Pipes::{GetNamedPipeInfo, PIPE_SERVER_END};
    let server_end = |h| {
        let mut flags = 0u32;
        // SAFETY: a pipe's handle its caller holds; the flags alone asked for.
        let got = unsafe {
            GetNamedPipeInfo(
                h,
                &mut flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (got != 0).then_some(flags & PIPE_SERVER_END != 0)
    };
    let name = |h| {
        // A FILE_NAME_INFO's length, then its name's UTF-16 units, in 4 KiB.
        let mut buf = [0u32; 1024];
        let room = u32::try_from(std::mem::size_of_val(&buf)).unwrap_or(0);
        // SAFETY: a buffer of `room` bytes, aligned for FILE_NAME_INFO, for a handle its
        // caller holds.
        if unsafe { GetFileInformationByHandleEx(h, FileNameInfo, buf.as_mut_ptr().cast(), room) } == 0 {
            return None;
        }
        let bytes = usize::try_from(*buf.first()?).ok()?;
        let units: Vec<u16> = buf
            .get(1..)?
            .iter()
            .flat_map(|w| [(*w & 0xFFFF) as u16, (*w >> 16) as u16])
            .collect();
        units.get(..bytes / 2).map(<[u16]>::to_vec)
    };
    server_end(ours) == Some(true)
        && server_end(theirs) == Some(false)
        && name(ours).is_some_and(|n| name(theirs) == Some(n))
}
