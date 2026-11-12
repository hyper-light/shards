//! A private nonblocking pipe; poll spends the original finite deadline.
#![allow(unsafe_code)]

use std::io::{self, Read as _};
use std::os::fd::AsRawFd as _;
use std::time::Instant;

use super::{buffer, io_error, pending};
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

    pub fn frame(&mut self, deadline: Instant) -> Result<&[u8], Refusal> {
        let reader = self
            .pipe
            .as_mut()
            .ok_or(Refusal::GpuProbe(GpuProbeError::Unavailable))?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
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
            // poll uses milliseconds; round UP once, then recheck the absolute
            // deadline. The i32 ceiling is its ABI, not a retry/tuning interval.
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(pending());
            }
            let millis = left
                .as_millis()
                .checked_add(u128::from(!left.subsec_nanos().is_multiple_of(1_000_000)))
                .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
            let timeout = i32::try_from(millis).unwrap_or(i32::MAX);
            let mut event = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one initialized stack pollfd, live private descriptor, and
            // finite native timeout; poll retains no memory after returning.
            let ready = unsafe { libc::poll(&mut event, 1, timeout) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(io_error("GPU report readiness", error));
                }
            } else if event.revents & libc::POLLNVAL != 0 {
                return Err(Refusal::Launch(crate::LaunchError::Observer));
            }
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
