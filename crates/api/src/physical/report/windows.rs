//! Windows anonymous pipes are synchronous. This private local pipe has an
//! overlapped read end; only its synchronous owned writer leaves the API. It is a
//! named pipe, as Windows makes its anonymous pipes, with a name of this process's
//! alone (`private_name`): one named for its scope refused a second report of that
//! scope, its one instance still live, ERROR_PIPE_BUSY (PM M533).
#![allow(unsafe_code)]

use std::cell::UnsafeCell;
use std::io;
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::time::Instant;

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED,
        GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
        ReadFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED},
        Pipes::{
            CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Threading::{CreateEventW, INFINITE, ResetEvent, WaitForSingleObject},
    },
};

use super::{io_error, pending};
use crate::{GpuProbeError, Refusal};

#[derive(Debug, Clone, Copy)]
enum Operation {
    Idle,
    Pending,
    Closed,
}

pub(super) struct Reader {
    pipe: ManuallyDrop<OwnedHandle>,
    event: ManuallyDrop<OwnedHandle>,
    // Both backing allocations are fallible and never resize while IO is pending.
    // A one-element Vec pins OVERLAPPED without an infallible Box allocation.
    operation: ManuallyDrop<Vec<UnsafeCell<OVERLAPPED>>>,
    bytes: ManuallyDrop<Vec<UnsafeCell<u8>>>,
    used: usize,
    state: Operation,
    #[cfg(test)]
    cancel_errors: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("GpuReportReader")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

// SAFETY: owned kernel handles are Send. The only embedded raw value is the
// event HANDLE in heap-stable OVERLAPPED; no pointer targets this movable struct.
// Exclusive &mut access/Shared's mutex serializes calls. The existing watcher
// retains Shared until positive completion. Drop never waits; an unexpected
// Pending drop retains all kernel-referenced resources rather than freeing them.
unsafe impl Send for Reader {}

fn owned(raw: *mut core::ffi::c_void, operation: &'static str) -> Result<OwnedHandle, Refusal> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(io_error(operation, io::Error::last_os_error()));
    }
    // SAFETY: callers pass a newly created, uniquely owned non-null/invalid handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

impl Reader {
    pub fn new(byte_count: usize) -> Result<(Self, io::PipeWriter), Refusal> {
        use core::fmt::Write as _;
        let allocation = || Refusal::GpuProbe(GpuProbeError::Allocation);
        let mut text = String::new();
        let prefix = r"\\.\pipe\shards-api-gpu-";
        // The nonce's 16 hex digits, a dash, and the counter's at most 20 decimal ones.
        let numbers = "ffffffffffffffff-18446744073709551615".len();
        text.try_reserve_exact(prefix.len().checked_add(numbers).ok_or_else(allocation)?)
            .map_err(|_| allocation())?;
        let (nonce, n) = crate::client::private_name();
        write!(&mut text, "{prefix}{nonce:016x}-{n}").map_err(|_| allocation())?;
        let mut name = Vec::new();
        name.try_reserve_exact(text.len().checked_add(1).ok_or_else(allocation)?)
            .map_err(|_| allocation())?;
        name.extend(text.encode_utf16());
        name.push(0);
        let capacity = u32::try_from(byte_count).map_err(|_| Refusal::GpuProbe(GpuProbeError::Count))?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(byte_count).map_err(|_| allocation())?;
        bytes.resize_with(byte_count, || UnsafeCell::new(0u8));
        let mut operation = Vec::new();
        operation.try_reserve_exact(1).map_err(|_| allocation())?;
        // SAFETY: null security attributes/name create a private non-inheritable
        // manual-reset event. It is retained through every overlapped operation.
        let event = owned(
            unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) },
            "GPU report event",
        )?;
        operation.push(UnsafeCell::new(OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..OVERLAPPED::default()
        }));
        // SAFETY: terminated name, derived bounded buffers, one instance, and
        // non-inheritable server handle. FIRST_PIPE_INSTANCE refuses name reuse;
        // REJECT_REMOTE_CLIENTS prevents a network client from supplying reports.
        let pipe = owned(
            unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    0,
                    capacity,
                    0,
                    core::ptr::null(),
                )
            },
            "GPU report pipe",
        )?;
        // SAFETY: this opens the same newly created local pipe synchronously for
        // writing, without inherited handles, retries, external services or waits.
        let writer = owned(
            unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    core::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    core::ptr::null_mut(),
                )
            },
            "GPU report writer",
        )?;
        let mut client = 0u32;
        // SAFETY: the owned server is connected by the successful CreateFileW;
        // the native API writes only this initialized process-id scalar.
        if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut client) } == 0 {
            return Err(io_error(
                "GPU report private connection",
                io::Error::last_os_error(),
            ));
        }
        if client != std::process::id() {
            return Err(Refusal::Launch(crate::LaunchError::Observer));
        }
        Ok((
            Self {
                pipe: ManuallyDrop::new(pipe),
                event: ManuallyDrop::new(event),
                operation: ManuallyDrop::new(operation),
                bytes: ManuallyDrop::new(bytes),
                used: 0,
                state: Operation::Idle,
                #[cfg(test)]
                cancel_errors: std::sync::atomic::AtomicUsize::new(0),
            },
            io::PipeWriter::from(writer),
        ))
    }

    fn completed(&mut self, count: u32) -> Result<bool, Refusal> {
        self.state = Operation::Idle;
        if count == 0 {
            return Ok(true);
        }
        let count = usize::try_from(count).map_err(|_| Refusal::GpuProbe(GpuProbeError::Count))?;
        self.used = self
            .used
            .checked_add(count)
            .filter(|used| *used <= self.bytes.len())
            .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
        Ok(false)
    }

    fn await_read(&mut self, deadline: Instant) -> Result<bool, Refusal> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.cancel()?;
                return Err(pending());
            }
            // DWORD milliseconds; MAX is INFINITE, so the largest finite value
            // is MAX-1. Longer finite budgets are split only at that ABI limit.
            let millis = left
                .as_millis()
                .checked_add(u128::from(!left.subsec_nanos().is_multiple_of(1_000_000)))
                .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
            let timeout = u32::try_from(millis).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
            let operation = self
                .operation
                .first()
                .ok_or(Refusal::Launch(crate::LaunchError::Observer))?
                .get();
            let mut count = 0;
            // SAFETY: the same live server and heap-stable OVERLAPPED submitted
            // for this pending read. Its event/buffer remain owned and untouched.
            if unsafe { GetOverlappedResultEx(self.pipe.as_raw_handle(), operation, &mut count, timeout, 0) }
                != 0
            {
                return self.completed(count);
            }
            // SAFETY: immediately reads this thread's failed native call status.
            let error = unsafe { GetLastError() };
            match error {
                WAIT_TIMEOUT | ERROR_IO_INCOMPLETE if Instant::now() < deadline => continue,
                WAIT_TIMEOUT | ERROR_IO_INCOMPLETE => {
                    self.cancel()?;
                    return Err(pending());
                }
                ERROR_BROKEN_PIPE => {
                    self.state = Operation::Idle;
                    return Ok(true);
                }
                ERROR_OPERATION_ABORTED => {
                    self.state = Operation::Idle;
                    return Err(io_error(
                        "GPU report cancelled completion",
                        io::Error::from_raw_os_error(i32::from_ne_bytes(error.to_ne_bytes())),
                    ));
                }
                _ => {
                    // Unknown completion-call errors do not establish terminal
                    // storage ownership. Keep Pending for the existing watcher.
                    return Err(io_error(
                        "GPU report completion",
                        io::Error::from_raw_os_error(i32::from_ne_bytes(error.to_ne_bytes())),
                    ));
                }
            }
        }
    }

    pub fn frame(&mut self, deadline: Instant) -> Result<&[u8], Refusal> {
        loop {
            if Instant::now() >= deadline {
                self.cancel()?;
                return Err(pending());
            }
            match self.state {
                Operation::Closed => return Err(Refusal::GpuProbe(GpuProbeError::Unavailable)),
                Operation::Pending => {
                    if self.await_read(deadline)? {
                        break;
                    }
                    continue;
                }
                Operation::Idle => {}
            }
            let count = self
                .bytes
                .len()
                .checked_sub(self.used)
                .filter(|count| *count > 0)
                .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
            let count = u32::try_from(count).map_err(|_| Refusal::GpuProbe(GpuProbeError::Count))?;
            let output = self
                .bytes
                .as_ptr()
                .wrapping_add(self.used)
                .cast::<u8>()
                .cast_mut();
            let operation = self
                .operation
                .first()
                .ok_or(Refusal::Launch(crate::LaunchError::Observer))?
                .get();
            // SAFETY: no previous operation is pending. Reset this retained
            // manual event before the next read's immutable heap addresses escape.
            if unsafe { ResetEvent(self.event.as_raw_handle()) } == 0 {
                return Err(io_error("GPU report reset", io::Error::last_os_error()));
            }
            // SAFETY: Idle proves no pending operation aliases this cell.
            unsafe {
                operation.write(OVERLAPPED {
                    hEvent: self.event.as_raw_handle(),
                    ..OVERLAPPED::default()
                });
            }
            // SAFETY: writable reserved buffer and OVERLAPPED have stable heap
            // addresses until positive completion. No Rust access occurs pending.
            let read = unsafe {
                ReadFile(
                    self.pipe.as_raw_handle(),
                    output,
                    count,
                    core::ptr::null_mut(),
                    operation,
                )
            };
            if read != 0 {
                self.state = Operation::Pending;
                continue;
            }
            // SAFETY: immediately reads this thread's failed native call status.
            match unsafe { GetLastError() } {
                ERROR_IO_PENDING => self.state = Operation::Pending,
                ERROR_BROKEN_PIPE => break,
                _ => return Err(io_error("GPU report read", io::Error::last_os_error())),
            }
        }
        if self.used > self.bytes.len() || matches!(self.state, Operation::Pending) {
            return Err(Refusal::Launch(crate::LaunchError::Observer));
        }
        // SAFETY: no operation is pending; UnsafeCell<u8> has u8's layout, and
        // the initialized, stable allocation contains at least used bytes. This
        // slice borrow prevents another mutable operation until parsing ends.
        Ok(unsafe { std::slice::from_raw_parts(self.bytes.as_ptr().cast::<u8>(), self.used) })
    }

    fn cancel(&self) -> Result<(), Refusal> {
        if matches!(self.state, Operation::Pending) {
            #[cfg(test)]
            if self
                .cancel_errors
                .try_update(
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                    |left| left.checked_sub(1),
                )
                .is_ok()
            {
                return Err(io_error(
                    "injected private report cancellation",
                    io::ErrorKind::Other.into(),
                ));
            }
            let operation = self
                .operation
                .first()
                .ok_or(Refusal::Launch(crate::LaunchError::Observer))?
                .get();
            // SAFETY: the same submitted raw cell pointer and owned live handle.
            // No &OVERLAPPED exists while the kernel may mutate its storage.
            if unsafe { CancelIoEx(self.pipe.as_raw_handle(), operation) } == 0 {
                // SAFETY: reads this thread's immediately preceding failure.
                let error = unsafe { GetLastError() };
                // Not found can race completed IO, but is NOT completion proof.
                if error != ERROR_NOT_FOUND {
                    return Err(io_error(
                        "GPU report cancellation request",
                        io::Error::from_raw_os_error(i32::from_ne_bytes(error.to_ne_bytes())),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), Refusal> {
        if matches!(self.state, Operation::Pending) {
            self.cancel()?;
            let operation = self
                .operation
                .first()
                .ok_or(Refusal::Launch(crate::LaunchError::Observer))?
                .get();
            let mut count = 0;
            // SAFETY: SAME stable cells/buffer/handles. Only the existing native
            // watcher performs this completion wait. A cancellation request is
            // insufficient; only success or a documented completed error closes.
            if unsafe { GetOverlappedResult(self.pipe.as_raw_handle(), operation, &mut count, 1) } == 0 {
                // SAFETY: reads this thread's immediately preceding failure.
                let error = unsafe { GetLastError() };
                if !matches!(error, ERROR_OPERATION_ABORTED | ERROR_BROKEN_PIPE) {
                    return Err(io_error(
                        "GPU report cleanup completion",
                        io::Error::from_raw_os_error(i32::from_ne_bytes(error.to_ne_bytes())),
                    ));
                }
            }
        }
        self.state = Operation::Closed;
        Ok(())
    }

    /// After an observation/cancellation error, subscribe to the operation's
    /// ACTUAL private completion event. A later kernel completion must wake this
    /// same watcher even when the last API owner has already gone.
    pub fn wait_for_completion(&mut self) -> Result<(), Refusal> {
        if matches!(self.state, Operation::Pending) {
            // SAFETY: this event is uniquely owned, not inherited or signalled by
            // callers; it was reset immediately before this one pending read.
            // Windows signals it only after that operation completes. Shared
            // retains the same cells/handles throughout this lifecycle wait.
            let returned = unsafe { WaitForSingleObject(self.event.as_raw_handle(), INFINITE) };
            if returned != WAIT_OBJECT_0 {
                return Err(if returned == windows_sys::Win32::Foundation::WAIT_FAILED {
                    io_error("GPU report native completion event", io::Error::last_os_error())
                } else {
                    Refusal::Launch(crate::LaunchError::Observer)
                });
            }
        }
        // No data is published after an earlier take/cleanup error. The unique
        // operation's positive native event returns storage ownership even if
        // its result status was unavailable; it is not native scope release.
        self.state = Operation::Closed;
        Ok(())
    }

    #[cfg(test)]
    pub fn fail_cancel_calls_for_test(&self, calls: usize) {
        self.cancel_errors
            .store(calls, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // Production cannot reach last-drop Pending: take is exposed only after
        // the watcher owns Shared, and that watcher retains it on cleanup error.
        // Keep a memory-safety backstop for an unexpected internal early drop:
        // retain its kernel-referenced resources rather than block a caller or
        // free live IO storage. Such retention is uncertainty, never a pass.
        if matches!(self.state, Operation::Pending) {
            return;
        }
        // SAFETY: Idle/Closed proves no pending kernel reference to either
        // allocation/handle. These are the four unique manually owned resources.
        unsafe {
            ManuallyDrop::drop(&mut self.operation);
            ManuallyDrop::drop(&mut self.bytes);
            ManuallyDrop::drop(&mut self.event);
            ManuallyDrop::drop(&mut self.pipe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owned writer remains open: only positive cancellation completion,
    /// never writer EOF or request success, can close this pending read storage.
    #[test]
    fn pending_private_read_keeps_cells_until_positive_cancel_completion() {
        let (mut reader, writer) = Reader::new(64).unwrap();
        let operation = reader.operation.first().unwrap().get();
        let buffer = reader.bytes.as_ptr();
        let deadline = Instant::now()
            .checked_add(crate::conformance::PATIENCE.get())
            .unwrap();
        assert!(matches!(
            reader.frame(deadline),
            Err(Refusal::GpuProbe(GpuProbeError::Pending))
        ));
        assert!(matches!(reader.state, Operation::Pending));
        assert_eq!(reader.operation.first().unwrap().get(), operation);
        assert_eq!(reader.bytes.as_ptr(), buffer);
        reader.finish().unwrap();
        assert!(matches!(reader.state, Operation::Closed));
        drop(writer);
    }
}
