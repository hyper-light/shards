//! A bounded positional effect on one already-selected regular object.
//!
//! Object checks are mechanisms, not grants. The canonical caller must retain
//! the actual owner-selected WRITE capability/current admission and reserve its
//! payload, audit and result resources. This leaf never resolves another path,
//! creates/truncates a file, issues authority or exposes its writable descriptor.

use std::borrow::Cow;
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};

use super::file;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Bounds,
    Object(file::ReadFailure),
    FileKind,
    Access,
    Append,
    Native(Option<i32>),
    WriteZero,
    Count,
}

/// Literal caller bytes, borrowed or already acquired. Neither form grants storage.
pub(crate) struct Request<'a> {
    offset: libc::off_t,
    bytes: Cow<'a, [u8]>,
}

impl<'a> Request<'a> {
    pub fn new(offset: u64, bytes: &'a [u8]) -> Result<Self, Failure> {
        Ok(Self {
            offset: checked_offset(offset, bytes.len())?,
            bytes: Cow::Borrowed(bytes),
        })
    }

    /// Return the original borrowed view or owned allocation without materializing
    /// a copy, including when `Rejected` returns the selected object and request.
    pub fn into_payload(self) -> Cow<'a, [u8]> {
        self.bytes
    }
}

impl Request<'static> {
    /// Retain the caller's initialized allocation through readiness and partial
    /// writes. Acquiring and charging that allocation remains the caller's duty.
    pub fn owned(offset: u64, bytes: Vec<u8>) -> Result<Self, RejectedPayload> {
        match checked_offset(offset, bytes.len()) {
            Ok(offset) => Ok(Self {
                offset,
                bytes: Cow::Owned(bytes),
            }),
            Err(failure) => Err(RejectedPayload { bytes, failure }),
        }
    }
}

fn checked_offset(offset: u64, bytes: usize) -> Result<libc::off_t, Failure> {
    let end = offset
        .checked_add(u64::try_from(bytes).map_err(|_| Failure::Bounds)?)
        .ok_or(Failure::Bounds)?;
    let offset = libc::off_t::try_from(offset).map_err(|_| Failure::Bounds)?;
    libc::off_t::try_from(end).map_err(|_| Failure::Bounds)?;
    Ok(offset)
}

pub(crate) struct RejectedPayload {
    pub bytes: Vec<u8>,
    pub failure: Failure,
}

impl std::fmt::Debug for RejectedPayload {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("RejectedPayload")
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Rejected<'a> {
    pub object: OwnedFd,
    pub request: Request<'a>,
    pub failure: Failure,
}

impl std::fmt::Debug for Rejected<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Rejected")
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

/// One actual held object and its committed-prefix cursor, never a path cache.
pub(crate) struct Prepared<'a> {
    object: OwnedFd,
    request: Request<'a>,
    identity: (u64, u64),
    accepted: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    Complete,
    Cancelled,
    Failed(Failure),
}

/// Kernel-accepted prefix only; neither persistence nor transactional rollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub requested: usize,
    pub accepted: usize,
    pub end: End,
}

pub(crate) enum Step<'a> {
    /// The same held object/bytes remain; recheck current owner/cancel before
    /// another call. This is not an instruction to blindly replay the request.
    Partial(Prepared<'a>),
    Terminal(Outcome),
}

impl<'a> Prepared<'a> {
    /// The caller selected `object` through the canonical granted common-view
    /// resolver. Native writable mode is checked only for pwrite correctness;
    /// it cannot replace that independent selection and current-owner decision.
    pub fn existing(object: OwnedFd, request: Request<'a>) -> Result<Self, Rejected<'a>> {
        let check = || {
            let metadata = file::stat(object.as_fd()).map_err(Failure::Object)?;
            if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(Failure::FileKind);
            }
            // SAFETY: F_GETFL observes this live held descriptor, without changing it.
            let flags = unsafe { libc::fcntl(object.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(Failure::Native(std::io::Error::last_os_error().raw_os_error()));
            }
            if flags & libc::O_ACCMODE == libc::O_RDONLY || flags & libc::O_PATH != 0 {
                return Err(Failure::Access);
            }
            // Linux pwrite on O_APPEND appends regardless of its explicit offset.
            // Never clear flags on a shared open file description to repair it.
            if flags & libc::O_APPEND != 0 {
                return Err(Failure::Append);
            }
            Ok((metadata.st_dev, metadata.st_ino))
        };
        match check() {
            Ok(identity) => Ok(Self {
                object,
                request,
                identity,
                accepted: 0,
            }),
            Err(failure) => Err(Rejected {
                object,
                request,
                failure,
            }),
        }
    }

    pub fn identity(&self) -> (u64, u64) {
        self.identity
    }

    pub fn requested(&self) -> usize {
        self.request.bytes.len()
    }

    pub fn offset(&self) -> libc::off_t {
        self.request.offset
    }

    pub fn accepted(&self) -> usize {
        self.accepted
    }

    pub fn cancel(self) -> Outcome {
        self.finish(End::Cancelled)
    }

    fn finish(self, end: End) -> Outcome {
        Outcome {
            requested: self.request.bytes.len(),
            accepted: self.accepted,
            end,
        }
    }

    /// At most one native attempt, called only after the canonical SERVER's
    /// intent barrier and the SAME live owner's actual commit admission. Neither
    /// journal storage nor a manufactured permission bool belongs in the helper.
    pub fn commit(mut self) -> Step<'a> {
        let Some(remaining) = self.request.bytes.get(self.accepted..) else {
            return Step::Terminal(self.finish(End::Failed(Failure::Count)));
        };
        if remaining.is_empty() {
            return Step::Terminal(self.finish(End::Complete));
        }
        let offset = libc::off_t::try_from(self.accepted)
            .ok()
            .and_then(|n| self.request.offset.checked_add(n));
        let Some(offset) = offset else {
            return Step::Terminal(self.finish(End::Failed(Failure::Bounds)));
        };
        // SAFETY: the held checked regular object and selected literal bytes
        // remain live; the complete offset range was checked before any effect.
        let written = unsafe {
            libc::pwrite(
                self.object.as_raw_fd(),
                remaining.as_ptr().cast(),
                remaining.len(),
                offset,
            )
        };
        if written < 0 {
            let error = std::io::Error::last_os_error().raw_os_error();
            return Step::Terminal(self.finish(End::Failed(Failure::Native(error))));
        }
        if written == 0 {
            return Step::Terminal(self.finish(End::Failed(Failure::WriteZero)));
        }
        let written = usize::try_from(written).ok().filter(|&n| n <= remaining.len());
        let Some(accepted) = written.and_then(|n| self.accepted.checked_add(n)) else {
            return Step::Terminal(self.finish(End::Failed(Failure::Count)));
        };
        self.accepted = accepted;
        if self.accepted == self.request.bytes.len() {
            Step::Terminal(self.finish(End::Complete))
        } else {
            Step::Partial(self)
        }
    }
}

#[cfg(test)]
mod tests;
