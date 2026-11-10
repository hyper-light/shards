//! Test-only C-stream-lock experiment. This is not a product reporting adapter.

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("this unadopted prototype is scoped to native macOS arm64");

use std::fmt::{self, Display};
use std::io;

// SAFETY: declarations match root-reviewed Apple macOS26.4 SDK _stdio.h;
// libc0.2.189 supplies FILE but not these Darwin symbols. See README sources.
unsafe extern "C" {
    static mut __stderrp: *mut libc::FILE;
    fn flockfile(stream: *mut libc::FILE);
    fn funlockfile(stream: *mut libc::FILE);
}

#[derive(Debug)]
pub enum ReportError {
    StreamUnavailable,
    Io(io::Error),
    Formatter(fmt::Error),
}

struct HeldStream(*mut libc::FILE);

impl Drop for HeldStream {
    fn drop(&mut self) {
        // SAFETY: constructed only after flockfile; report's caller keeps this
        // exact FILE live and unchanged through formatting and this unlock.
        unsafe { funlockfile(self.0) };
    }
}

struct Fd2 {
    failure: Option<io::Error>,
}

impl fmt::Write for Fd2 {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.failure.is_some() {
            return Err(fmt::Error);
        }
        if let Err(error) = write_all(text.as_bytes()) {
            self.failure = Some(error);
            return Err(fmt::Error);
        }
        Ok(())
    }
}

fn write_all(mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // POSIX write's representable ssize_t count, not a message-size budget.
        let count = bytes.len().min(libc::ssize_t::MAX as usize);
        // SAFETY: count bytes remain live for the call. fd2 is an integer target,
        // not an assumed-open BorrowedFd or an owning File; EBADF is an I/O error.
        let written = unsafe { libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), count) };
        if written == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        let written = usize::try_from(written).map_err(|_| io::ErrorKind::InvalidData)?;
        bytes = bytes.get(written..).ok_or(io::ErrorKind::InvalidData)?;
    }
    Ok(())
}

/// Formats the existing `shards: {message}\n` representation directly to fd2.
///
/// # Safety
/// The process's stderr FILE must remain live, with no fclose, freopen or pointer
/// replacement during this call, including from another thread or Display. fd2
/// itself may be closed; no descriptor is borrowed, duplicated or closed here.
/// This lock does not participate in Rust's private standard-stderr lock.
pub unsafe fn report(message: impl Display) -> Result<(), ReportError> {
    // SAFETY: caller guarantees the live process FILE/pointer lifetime above.
    let stream = unsafe { __stderrp };
    if stream.is_null() {
        return Err(ReportError::StreamUnavailable);
    }
    // SAFETY: same FILE lifetime; flockfile's recursive lock covers all fragments.
    unsafe { flockfile(stream) };
    let _held = HeldStream(stream);
    let mut sink = Fd2 { failure: None };
    let formatted = fmt::write(&mut sink, format_args!("shards: {message}\n"));
    match (sink.failure, formatted) {
        // Retain the first actual I/O failure even if Display ignores fmt::Error.
        (Some(error), _) => Err(ReportError::Io(error)),
        (None, Err(error)) => Err(ReportError::Formatter(error)),
        (None, Ok(())) => Ok(()),
    }
}
