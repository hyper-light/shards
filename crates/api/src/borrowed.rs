//! Connect to an existing identity without acquiring its guest lifetime.
use std::io;
use std::time::{Duration, Instant};

use crate::{HandshakeError, Identity, Stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPhase {
    Open,
    Start,
    Exit,
}

#[derive(Debug)]
pub enum ProcessError {
    InvalidPid,
    Changed { expected: u64, found: u64 },
    Ended,
    Io { phase: ProcessPhase, source: io::Error },
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPid => f.write_str("pid is outside the OS process-id range"),
            Self::Changed { expected, found } => write!(f, "process started at {found}, not {expected}"),
            Self::Ended => f.write_str("process has ended"),
            Self::Io { phase, source } => write!(
                f,
                "{phase:?}: {:?} (OS {:?})",
                source.kind(),
                source.raw_os_error()
            ),
        }
    }
}
impl std::error::Error for ProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Borrowed connection failures preserve their phase and cause without allocating.
#[derive(Debug)]
pub enum BorrowError {
    Expired,
    DeadlineOverflow,
    Process { pid: u32, cause: ProcessError },
    Channel { port: u32, cause: HandshakeError },
    Allocation,
}

impl std::fmt::Display for BorrowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => f.write_str("borrowed connection deadline expired"),
            Self::DeadlineOverflow => f.write_str("borrowed connection deadline is not representable"),
            Self::Process { pid, cause } => write!(f, "pid {pid}: {cause}"),
            Self::Channel { port, cause } => write!(f, "vsock port {port}: {cause}"),
            Self::Allocation => f.write_str("cannot allocate borrowed connection storage"),
        }
    }
}
impl std::error::Error for BorrowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Process { cause, .. } => Some(cause),
            Self::Channel { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

/// Connect to an already running guest's port without owning its lifetime.
///
/// Zero patience is expired before I/O; an overflowing absolute deadline is refused
/// before dial. One finite deadline covers process validation, connect, request and
/// every reply byte. Unlike [`crate::Guest::connect`], there is no retry or unbounded
/// zero/MAX mode. Dropping the returned stream closes only that connection.
pub fn connect_borrowed(
    identity: &Identity,
    port: u32,
    patience: Duration,
) -> Result<Box<dyn Stream>, BorrowError> {
    let began = Instant::now();
    if patience.is_zero() {
        return Err(BorrowError::Expired);
    }
    let deadline = began.checked_add(patience).ok_or(BorrowError::DeadlineOverflow)?;
    #[cfg(unix)]
    let held = crate::taken::Held::find_checked(identity.pid, identity.started);
    #[cfg(windows)]
    let held = windows::Held::find(identity.pid, identity.started);
    let held = held.map_err(|cause| BorrowError::Process {
        pid: identity.pid,
        cause,
    })?;
    let mut cost = Duration::ZERO;
    let stream =
        crate::channel::handshake(&identity.vsock, port, Some(deadline), &mut cost).map_err(|cause| {
            if cause.allocation() {
                BorrowError::Allocation
            } else {
                BorrowError::Channel { port, cause }
            }
        })?;
    held.check().map_err(|cause| BorrowError::Process {
        pid: identity.pid,
        cause,
    })?;
    if Instant::now() >= deadline {
        return Err(BorrowError::Expired);
    }
    let stream = stream.boxed().map_err(|_| BorrowError::Allocation)?;
    if Instant::now() >= deadline {
        return Err(BorrowError::Expired);
    }
    Ok(stream)
}

#[cfg(test)]
#[path = "../../../docs/research/measurements/managed-borrowed/green.rs"]
mod tests;

#[cfg(windows)]
mod windows {
    use super::{ProcessError, ProcessPhase, io};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    pub(super) struct Held(OwnedHandle);
    impl Held {
        pub(super) fn find(pid: u32, expected: u64) -> Result<Self, ProcessError> {
            // SAFETY: query/synchronize only, non-inheritable. No termination right.
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                return Err(ProcessError::Io {
                    phase: ProcessPhase::Open,
                    source: io::Error::last_os_error(),
                });
            }
            // SAFETY: the successful OpenProcess handle is owned here alone; all paths
            // close through OwnedHandle, never a watcher or Guest lifetime owner.
            let held = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
            let found =
                crate::client::created_handle(held.0.as_raw_handle()).map_err(|source| ProcessError::Io {
                    phase: ProcessPhase::Start,
                    source,
                })?;
            if found != expected {
                return Err(ProcessError::Changed { expected, found });
            }
            held.check()?;
            Ok(held)
        }

        pub(super) fn check(&self) -> Result<(), ProcessError> {
            // SAFETY: the held query+synchronize handle stays open throughout this
            // zero-time state check. This is not a periodic probe or a lifecycle wait.
            match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
                WAIT_TIMEOUT => Ok(()),
                WAIT_OBJECT_0 => Err(ProcessError::Ended),
                WAIT_FAILED => Err(ProcessError::Io {
                    phase: ProcessPhase::Exit,
                    source: io::Error::last_os_error(),
                }),
                _ => Err(ProcessError::Io {
                    phase: ProcessPhase::Exit,
                    source: io::ErrorKind::InvalidData.into(),
                }),
            }
        }
    }
}
