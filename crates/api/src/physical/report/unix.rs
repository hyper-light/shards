//! A private nonblocking pipe; poll spends the original deadline, if any, and waits on the
//! VM process's end beside it: the descriptor that holds that process (`super::Vm`), a pidfd
//! (Linux) or a kqueue with its exit registered (macOS), each readable once it has exited.
#![allow(unsafe_code)]

use std::io::{self, Read as _};
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::time::Instant;

use super::{Read, buffer, io_error, pending};
use crate::{GpuProbeError, Refusal};

#[derive(Debug)]
pub(super) struct Reader {
    pipe: Option<io::PipeReader>,
    bytes: Vec<u8>,
    used: usize,
}

impl Reader {
    pub fn new(byte_count: usize) -> Result<(Self, io::PipeWriter), Refusal> {
        let bytes = buffer(byte_count)?;
        let (reader, writer) = io::pipe().map_err(|e| io_error("GPU report pipe", e))?;
        let fd = reader.as_raw_fd();
        // SAFETY: fd is a live private pipe, owned here; no concurrent reader or
        // foreign launch exists yet. Existing descriptor flags are preserved.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io_error("GPU report flags", io::Error::last_os_error()));
        }
        // SAFETY: the same owned descriptor/flags are valid and O_NONBLOCK only
        // affects this read end. The separately owned writer remains blocking.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io_error("GPU report nonblocking", io::Error::last_os_error()));
        }
        Ok((
            Self {
                pipe: Some(reader),
                bytes,
                used: 0,
            },
            writer,
        ))
    }

    /// The frame, read to its writer's close, by `deadline` where there is one. Its VM
    /// process's end, readable on `exit` (none: it had ended before it was held), is answered
    /// once the pipe holds nothing more to read and no close: a frame its writer closed before
    /// the end is still the frame.
    pub fn frame(
        &mut self,
        deadline: Option<Instant>,
        exit: Option<BorrowedFd<'_>>,
    ) -> Result<Read<'_>, Refusal> {
        // Its watcher lets the pipe go only after the VM process's end.
        let Some(reader) = self.pipe.as_mut() else {
            return Ok(Read::Ended);
        };
        let mut ended = false;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(pending());
            }
            let available = self
                .bytes
                .get_mut(self.used..)
                .filter(|tail| !tail.is_empty())
                .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
            match reader.read(available) {
                Ok(0) => {
                    return self
                        .bytes
                        .get(..self.used)
                        .map(Read::Frame)
                        .ok_or(Refusal::GpuProbe(GpuProbeError::Count));
                }
                Ok(read) => {
                    self.used = self
                        .used
                        .checked_add(read)
                        .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(io_error("GPU report read", error)),
            }
            // Ended as it was seen, or before it was held: the pipe read once more.
            let exit = match (ended, exit) {
                (false, Some(exit)) => exit,
                _ => return Ok(Read::Ended),
            };
            // poll uses milliseconds; round UP once, then recheck the absolute
            // deadline. The i32 ceiling is its ABI, not a retry/tuning interval;
            // -1 is poll(2)'s own "no timeout", for a wait with no deadline.
            let timeout = match deadline {
                None => -1,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(pending());
                    }
                    let millis = left
                        .as_millis()
                        .checked_add(u128::from(!left.subsec_nanos().is_multiple_of(1_000_000)))
                        .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
                    i32::try_from(millis).unwrap_or(i32::MAX)
                }
            };
            let mut events = [reader.as_raw_fd(), exit.as_raw_fd()].map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            // SAFETY: two initialized stack pollfds, of the live private pipe and the
            // descriptor that holds the VM process, their count the array's; poll retains no
            // memory after returning.
            let ready = unsafe { libc::poll(events.as_mut_ptr(), 2, timeout) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(io_error("GPU report readiness", error));
                }
                continue;
            }
            let [report, end] = events;
            if (report.revents | end.revents) & libc::POLLNVAL != 0 {
                return Err(Refusal::Launch(crate::LaunchError::Observer));
            }
            // Readable, or anything else reported on it: the process has exited. The report
            // pipe is read once more before the end is answered.
            ended = end.revents != 0;
        }
    }

    pub fn finish(&mut self) -> Result<(), Refusal> {
        self.pipe.take();
        Ok(())
    }

    pub fn wait_for_completion(&mut self) -> Result<(), Refusal> {
        self.finish()
    }
}
