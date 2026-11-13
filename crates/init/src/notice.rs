//! Init's notice built-in (`run::builtin::NOTICE`, D235): what the host saw change in the
//! directories it shares, told to the guest's own inotify. Linux's virtio-fs driver has no
//! notification queue (fs/fuse/virtio_fs.c negotiates no feature), and FUSE tells fsnotify
//! nothing the server says, so a host's edit wakes no watch in the guest by itself. What
//! does is a change made through the guest's own VFS: each named file is touched, its
//! modification time set to now and its access time left (utimensat(2), `UTIME_OMIT` and
//! `UTIME_NOW`), which the kernel tells as `IN_MODIFY` (include/linux/fsnotify.h,
//! `fsnotify_change`, a modification time alone), to its own watches and to its
//! directory's. Done as [`shards_abi::NOTICE_ID`], which the share's server answers with
//! the file as it is and sets nothing of on the host. A file the host removed is no longer
//! there to touch: the guest's name of it, which its kernel may trust for a second, is
//! looked up again, and its directory touched.
//!
//! Each share is reached through a mount of its tag of this process's own, attached
//! nowhere (the new mount API), so that no mount namespace gains one; a second mount of a
//! tag is its first's file system (fs/fuse/virtio_fs.c, `virtio_fs_test_super`), and a
//! watch is of the inode, whichever mount it was set through. So only a domain or a
//! workload that has the share mounted, and watches there, is told: what is plugged into
//! nothing is told no one. Its mounts are let go of before each wait for more, so that
//! none outlives its device's unplug.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

use shards_abi::run::notice::{self, Decoded};

/// What a read of the host's notices takes at most: a pipe's capacity on Linux since
/// 2.6.11, 16 pages (pipe(7)), which is what one write of the host's can have left in it.
const READ: usize = 16 * 4096;

/// The longest record: a virtio-fs tag's most (virtio 1.3 §5.11.4, 36 bytes), a path's
/// (`PATH_MAX`, include/uapi/linux/limits.h, 4096 with its NUL) and their NULs. Past it, a
/// record never ends: the host's notices are not records.
const RECORD_MOST: usize = 36 + 1 + 4096 + 1;

/// Tells the guest's inotify of each notice `input` holds, until it closes; why one could
/// not be told on `out`, once a tag.
pub fn serve(input: &mut impl Read, out: &mut impl Write) -> io::Result<()> {
    // SAFETY: setfsgid(2) of this single-threaded child's own filesystem group; the call
    // with -1 changes nothing and says which it is now.
    let set = unsafe {
        libc::setfsgid(shards_abi::NOTICE_ID);
        libc::setfsgid(u32::MAX)
    };
    if u32::try_from(set) != Ok(shards_abi::NOTICE_ID) {
        return Err(io::Error::other("its filesystem group could not be set"));
    }
    let mut mounts = Mounts::default();
    let mut failed: Vec<Vec<u8>> = Vec::new();
    each_notice(input, |notice| match notice {
        Some((tag, path)) => {
            if let Err(e) = mounts.touch(tag, path)
                && !failed.iter().any(|t| t == tag)
            {
                failed.push(tag.to_vec());
                let _ = writeln!(out, "{}: {e}", String::from_utf8_lossy(tag));
            }
        }
        // Let go of before the next wait.
        None => mounts = Mounts::default(),
    })
}

/// Calls `each` with each record `input` holds, its tag and its path, in order, and with
/// none once those of a read are told, until `input` closes; an error where it holds what
/// is not records.
fn each_notice(input: &mut impl Read, mut each: impl FnMut(Option<(&[u8], &[u8])>)) -> io::Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; READ];
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        pending.extend_from_slice(buf.get(..n).unwrap_or_default());
        let mut at = 0;
        loop {
            match notice::decode(pending.get(at..).unwrap_or_default()) {
                Decoded::Record { name, path, rest } => {
                    each(Some((name, path)));
                    at = pending.len().saturating_sub(rest.len());
                }
                Decoded::Partial => break,
                Decoded::Malformed => return Err(io::Error::other("a notice that is not one")),
            }
        }
        each(None);
        pending.drain(..at.min(pending.len()));
        if pending.len() > RECORD_MOST {
            return Err(io::Error::other("a notice longer than any"));
        }
    }
}

/// The shares' mounts this process holds while it tells a batch of notices, by tag.
#[derive(Default)]
struct Mounts(HashMap<Vec<u8>, OwnedFd>);

impl Mounts {
    /// Touches `path` in the share whose tag is `tag`, or, gone, its nearest directory
    /// there.
    fn touch(&mut self, tag: &[u8], path: &[u8]) -> io::Result<()> {
        let root = match self.0.get(tag) {
            Some(root) => root,
            None => {
                let source = CString::new(tag).map_err(io::Error::other)?;
                let mounted = crate::hotplug::fs_mount(c"virtiofs", &source, false, 0)?;
                self.0.entry(tag.to_vec()).or_insert(mounted)
            }
        };
        let (dirs, name) = match path.iter().rposition(|&b| b == b'/') {
            Some(cut) => (
                path.get(..cut).unwrap_or_default(),
                path.get(cut + 1..).unwrap_or_default(),
            ),
            None => (&b""[..], path),
        };
        // Its directory, as far as it goes: never through a symlink, never above the share.
        let mut dir: Option<OwnedFd> = None;
        let mut whole = true;
        for part in dirs.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
            let from = dir.as_ref().map_or(root.as_raw_fd(), |d| d.as_raw_fd());
            match open_dir(from, part) {
                Ok(next) => dir = Some(next),
                Err(_) => {
                    whole = false;
                    break;
                }
            }
        }
        let at = dir.as_ref().map_or(root.as_raw_fd(), |d| d.as_raw_fd());
        if name.is_empty() || !whole {
            return touch(at, c".");
        }
        let name = CString::new(name).map_err(io::Error::other)?;
        match touch(at, &name) {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                forget(at, &name);
                touch(at, c".")
            }
            other => other,
        }
    }
}

/// Directory `name` in directory `dir`, as a path descriptor, never followed where it is a
/// symlink (`O_NOFOLLOW` with `O_DIRECTORY` refuses one).
fn open_dir(dir: libc::c_int, name: &[u8]) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(io::Error::other)?;
    loop {
        // SAFETY: openat(2) of a NUL-terminated name relative to a descriptor the caller
        // holds; no O_CREAT, so no mode is read.
        let fd = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: a descriptor just opened, owned here alone.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// `name` in `dir` touched: its modification time asked set to now, its access time left,
/// never following a symlink.
fn touch(dir: libc::c_int, name: &std::ffi::CStr) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
    ];
    // SAFETY: utimensat(2) of a NUL-terminated name relative to a descriptor the caller
    // holds, with two timespecs that outlive the call.
    if unsafe { libc::utimensat(dir, name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The guest's name `name` in `dir` looked up again, which an exclusive create asks of the
/// share whatever its kernel trusts (fs/fuse/dir.c, `fuse_dentry_revalidate`, on
/// `LOOKUP_EXCL`): a name the host removed is then no longer the guest's. The create is
/// refused, as every change a notice asks is (`NOTICE_ID`); where it is not, what it made
/// is let go of, and it stays as the share's server made it.
fn forget(dir: libc::c_int, name: &std::ffi::CStr) {
    // SAFETY: openat(2) of a NUL-terminated name relative to a descriptor the caller holds,
    // with a mode, as O_CREAT reads one.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0 as libc::c_uint,
        )
    };
    if fd >= 0 {
        // SAFETY: a descriptor just opened, owned here alone.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out a byte at a time.
    struct Trickle(Vec<u8>, usize);

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match (self.0.get(self.1), buf.first_mut()) {
                (Some(&b), Some(slot)) => {
                    *slot = b;
                    self.1 += 1;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    /// Records are told in order across reads that split them, each read's end said; and a
    /// stream that is not records ends the built-in, as one whose record never ends does.
    #[test]
    fn notices_are_told_whole_across_reads() {
        let mut records = Vec::new();
        assert!(notice::encode(b"shards0", b"a/b", &mut records));
        assert!(notice::encode(b"shards-plug1", b"", &mut records));
        let mut told = Vec::new();
        let read = each_notice(&mut Trickle(records.clone(), 0), |n| {
            told.push(n.map(|(t, p)| (t.to_vec(), p.to_vec())));
        });
        assert!(read.is_ok());
        let whole: Vec<_> = told.iter().flatten().cloned().collect();
        assert_eq!(
            whole,
            [
                (b"shards0".to_vec(), b"a/b".to_vec()),
                (b"shards-plug1".to_vec(), Vec::new())
            ]
        );
        // A batch's end after each read, a byte each here.
        assert_eq!(told.iter().filter(|n| n.is_none()).count(), records.len());
        let mut told = 0;
        let read = each_notice(&mut records.as_slice(), |n| told += usize::from(n.is_some()));
        assert!(read.is_ok());
        assert_eq!(told, 2);
        assert!(each_notice(&mut &b"t\0../x\0"[..], |_| {}).is_err());
        let endless = vec![b'x'; RECORD_MOST + 2];
        assert!(each_notice(&mut Trickle(endless, 0), |_| {}).is_err());
    }
}
