//! A dial by a guest's identity, borrowing no ownership of its life (D215).

use std::io;
use std::time::{Duration, Instant};

use crate::client::{Attempt, DialWait, handshake_waiting, pause};
use crate::{Identity, Refusal, Stream};

/// Connects to an existing guest's vsock port without taking ownership of the guest.
///
/// The process must still have `identity.pid` and `identity.started`. Its native process
/// reference is held while dialing, and its liveness checked before a stream is returned.
/// Neither a failed call nor dropping the returned stream stops the guest or removes its
/// socket or directory. The caller must already be permitted to open `identity.vsock`;
/// unlike [`crate::Guest::connect`], there is no `GuestSpec` port allowlist here.
///
/// One deadline covers connecting, sending the request, reading every answer byte and all
/// retries. Zero makes one nonblocking attempt; it never waits for an answer. An infinite
/// patience (`Duration::MAX`) or one outside this host's clock range is refused as Bounds.
/// A positive patience may expire before an attempt when checking the identity used it up.
/// Ordinary OS scheduling and the clock's/socket poll's resolution still apply.
pub fn connect_borrowed(
    identity: &Identity,
    port: u32,
    patience: Duration,
) -> Result<Box<dyn Stream>, Refusal> {
    let began = Instant::now();
    let deadline = began.checked_add(patience).filter(|_| patience != Duration::MAX);
    let deadline = deadline.ok_or_else(|| Refusal::Bounds {
        what: "patience",
        why: "a borrowed connect needs a finite patience within this host's clock range".into(),
    })?;
    let process = Process::find(identity)?;
    let wait = if patience.is_zero() {
        DialWait::Now
    } else {
        DialWait::Until(deadline)
    };
    loop {
        process.check(identity.pid)?;
        let mut cost = Duration::ZERO;
        let attempted = handshake_waiting(&identity.vsock, port, wait, &mut cost);
        process.check(identity.pid)?;
        let left = deadline.saturating_duration_since(Instant::now());
        let why = match attempted {
            Ok(stream) if patience.is_zero() || !left.is_zero() => return Ok(Box::new(stream)),
            Ok(_) => "its patience elapsed before the answer was returned".into(),
            Err(Attempt::Refused(why)) => return Err(Refusal::Channel { port, why }),
            Err(Attempt::Again(why)) => why,
        };
        if left.is_zero() {
            return Err(Refusal::Channel {
                port,
                why: format!("no OK within {patience:?}: {why}"),
            });
        }
        std::thread::sleep(pause(cost, began.elapsed(), left));
    }
}

/// Only native process references: no child, job, socket-directory owner or drop action.
struct Process {
    #[cfg(unix)]
    held: crate::taken::Held,
    #[cfg(windows)]
    held: std::os::windows::io::OwnedHandle,
}

impl Process {
    fn find(identity: &Identity) -> Result<Process, Refusal> {
        let gone = |why| Refusal::Gone {
            pid: identity.pid,
            why,
        };
        #[cfg(unix)]
        let held = crate::taken::Held::find(identity.pid, identity.started).map_err(gone)?;
        #[cfg(windows)]
        let held = {
            use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            };
            // SAFETY: OpenProcess returns a new, non-inheritable handle to this pid's
            // process. It grants querying and waiting only, never termination or ownership.
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    identity.pid,
                )
            };
            if raw.is_null() {
                return Err(gone(format!(
                    "opening its process: {}",
                    io::Error::last_os_error()
                )));
            }
            // SAFETY: the non-null handle OpenProcess just returned, owned here alone.
            let held = unsafe { OwnedHandle::from_raw_handle(raw) };
            let started =
                crate::client::created(&held).map_err(|e| gone(format!("reading its process start: {e}")))?;
            if started != identity.started {
                return Err(gone(format!(
                    "pid {} started at {started}, not {}: another process's since",
                    identity.pid, identity.started,
                )));
            }
            held
        };
        let process = Process { held };
        process.check(identity.pid)?;
        Ok(process)
    }

    fn check(&self, pid: u32) -> Result<(), Refusal> {
        self.exited()
            .map_err(|e| Refusal::Gone {
                pid,
                why: format!("checking its process: {e}"),
            })
            .and_then(|exited| {
                if exited {
                    Err(Refusal::Gone {
                        pid,
                        why: "its VM's process has ended".into(),
                    })
                } else {
                    Ok(())
                }
            })
    }

    fn exited(&self) -> io::Result<bool> {
        #[cfg(unix)]
        return self.held.has_exited();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            // SAFETY: this process reference is held open; zero queries without waiting.
            match unsafe { WaitForSingleObject(self.held.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                _ => Err(io::Error::last_os_error()),
            }
        }
    }
}
