use std::ffi::CStr;
use std::os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    Path,
    RootKind,
    FileKind,
    Length,
    Capacity,
    Io(Option<i32>),
}

fn native_error() -> ReadFailure {
    ReadFailure::Io(std::io::Error::last_os_error().raw_os_error())
}

/// A borrowed read authority. The caller retains the actual grant and root lifetime.
pub(crate) struct ReadRoot<'a>(BorrowedFd<'a>);

impl<'a> ReadRoot<'a> {
    /// `root` is init's D139 scratch descriptor, already selected by the private
    /// instance. Checking its kind does not grant another root or write authority.
    pub fn scratch(root: BorrowedFd<'a>) -> Result<Self, ReadFailure> {
        if stat(root)?.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(ReadFailure::RootKind);
        }
        Ok(Self(root))
    }

    pub fn open(&self, path: &CStr) -> Result<ReadFile, ReadFailure> {
        let fd = self.open_at(
            path,
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
        )?;
        let metadata = stat(std::os::fd::AsFd::as_fd(&fd))?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(ReadFailure::FileKind);
        }
        let size = u64::try_from(metadata.st_size).map_err(|_| ReadFailure::Length)?;
        Ok(ReadFile {
            fd,
            size,
            identity: (metadata.st_dev, metadata.st_ino),
        })
    }

    fn open_at(&self, path: &CStr, flags: i32) -> Result<OwnedFd, ReadFailure> {
        if path.to_bytes().is_empty() || path.to_bytes().first() == Some(&b'/') {
            return Err(ReadFailure::Path);
        }
        // Linux v6.18 include/uapi/linux/openat2.h: flags, mode, resolve. These
        // constraints are resolved atomically by the kernel, including racing paths.
        let how: [u64; 3] = [
            u64::try_from(flags).map_err(|_| ReadFailure::Path)?,
            0,
            crate::server::RESOLVE_BENEATH
                | crate::server::RESOLVE_NO_SYMLINKS
                | crate::server::RESOLVE_NO_MAGICLINKS
                | crate::server::RESOLVE_NO_XDEV,
        ];
        // SAFETY: held directory, live NUL-terminated path and exact open_how layout.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.0.as_raw_fd(),
                path.as_ptr(),
                how.as_ptr(),
                std::mem::size_of_val(&how),
            )
        };
        if fd < 0 {
            return Err(native_error());
        }
        let fd = i32::try_from(fd).map_err(|_| ReadFailure::Io(None))?;
        // SAFETY: successful openat2 transferred this descriptor to this caller alone.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

pub(super) fn stat(fd: BorrowedFd<'_>) -> Result<libc::stat, ReadFailure> {
    // SAFETY: all-zero stat is a valid output buffer for fstat.
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fd is live and metadata is writable for the complete native stat layout.
    if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut metadata) } != 0 {
        return Err(native_error());
    }
    Ok(metadata)
}

/// An opened regular file; later path replacement cannot change the selected object.
pub(crate) struct ReadFile {
    fd: OwnedFd,
    size: u64,
    identity: (u64, u64),
}

impl ReadFile {
    pub fn identity(&self) -> (u64, u64) {
        self.identity
    }

    /// Reservation may refuse an already oversized object before intent is synced.
    /// The effect still checks actual bytes because the held file can grow later.
    pub fn check_limit(&self, limit: usize) -> Result<(), ReadFailure> {
        if self.size > u64::try_from(limit).map_err(|_| ReadFailure::Length)? {
            return Err(ReadFailure::Length);
        }
        Ok(())
    }

    /// Append into the caller's existing reservation. The extra byte detects growth
    /// beyond the authorized payload; the metadata size is an early refusal only.
    pub fn append(&self, output: &mut Vec<u8>, limit: usize) -> Result<(), ReadFailure> {
        self.check_limit(limit)?;
        let start = output.len();
        let end = start
            .checked_add(limit)
            .and_then(|n| n.checked_add(1))
            .ok_or(ReadFailure::Length)?;
        if end > output.capacity() {
            return Err(ReadFailure::Capacity);
        }
        output.resize(end, 0);
        let result = self.fill(output, start, end, limit);
        match result {
            Ok(used) => {
                output.truncate(used);
                Ok(())
            }
            Err(error) => {
                // A refused/failed read never publishes a partial peer payload.
                output.truncate(start);
                Err(error)
            }
        }
    }

    fn fill(&self, output: &mut [u8], start: usize, end: usize, limit: usize) -> Result<usize, ReadFailure> {
        let mut used = start;
        loop {
            let target = output.get_mut(used..end).ok_or(ReadFailure::Length)?;
            // SAFETY: the held regular file and writable slice remain live throughout read.
            let read = unsafe { libc::read(self.fd.as_raw_fd(), target.as_mut_ptr().cast(), target.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(ReadFailure::Io(error.raw_os_error()));
            }
            if read == 0 {
                return Ok(used);
            }
            let read = usize::try_from(read).map_err(|_| ReadFailure::Length)?;
            used = used.checked_add(read).ok_or(ReadFailure::Length)?;
            if used.checked_sub(start).is_none_or(|n| n > limit) {
                return Err(ReadFailure::Length);
            }
        }
    }
}

#[cfg(test)]
mod tests;
