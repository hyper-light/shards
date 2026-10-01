//! A container log's segments (audit A12), which only the daemon makes: no VM reaches a
//! container's directory (D30), so a warm VM asks its daemon for each segment after the
//! first (`kind::LOG_SEGMENT`).

use std::fs;
use std::io;

/// A log's segments (audit A12): the first `log` and `log.idx`, then `log.1` and
/// `log.1.idx`, and on. A writer starts the next once the last would pass
/// [`LogRetention::size`](crate::spec::LogRetention) bytes, and removes the oldest past
/// [`LogRetention::files`](crate::spec::LogRetention). A
/// segment is there once its index is: it is made after its log, and removed before it.
/// Nothing is renamed, so a reader never pairs one segment's log with another's index,
/// and a segment's successor there says it is whole.
pub fn log_segment(seq: u64) -> (String, String) {
    if seq == 0 {
        ("log".into(), "log.idx".into())
    } else {
        (format!("log.{seq}"), format!("log.{seq}.idx"))
    }
}

/// Makes segment `seq` of the log in the container directory `dir` holds open, and removes
/// the oldest past `files` segments. A segment is there for readers once its index is, so
/// its log is made first, and its index goes first; the index there says the segment
/// before is done ([`log_segment`]). A segment already there is not taken, and one
/// that cannot be made whole leaves nothing of itself.
pub fn new_segment(dir: &fs::File, seq: u64, files: u64) -> io::Result<(fs::File, fs::File)> {
    let (log_name, index_name) = log_segment(seq);
    let log = open_in(dir, &log_name)?;
    let index = match open_in(dir, &index_name) {
        Ok(index) => index,
        Err(e) => {
            let _ = unlink_in(dir, &log_name);
            return Err(e);
        }
    };
    if let Some(gone) = seq.checked_sub(files) {
        let (log_name, index_name) = log_segment(gone);
        let _ = unlink_in(dir, &index_name);
        let _ = unlink_in(dir, &log_name);
    }
    Ok((log, index))
}

/// Makes `name` in the directory `dir` holds open, this user's alone, to append to; refused
/// if it is there.
fn open_in(dir: &fs::File, name: &str) -> io::Result<fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a name with NUL"))?;
    let flags = libc::O_WRONLY | libc::O_APPEND | libc::O_CLOEXEC | libc::O_CREAT | libc::O_EXCL;
    // SAFETY: openat(2) with a NUL-terminated name, relative to a directory held open.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o600 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just opened, owned by nothing else.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

/// Removes `name` from the directory `dir` holds open.
fn unlink_in(dir: &fs::File, name: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a name with NUL"))?;
    // SAFETY: unlinkat(2) with a NUL-terminated name, relative to a directory held open.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
