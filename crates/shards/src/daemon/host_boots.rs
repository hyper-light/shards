//! Cold boots held host-wide on Windows (PM M376): the slots every daemon on the host takes
//! a cold boot's turn in, beside its own count of them (`Boots`). WHP's cold boots of guests
//! of the host's CPUs slow one another past linearly, whichever daemon makes them: on CI's
//! 4-CPU windows-2022, two at once took 0.62 s each and eight 41.0 s, where guests of one
//! vCPU took 1.37 s eight at once; and CI's tests, a daemon each, made eight at once, two a
//! daemon. Each slot a named mutex in this logon session's namespace, `Local\NAME-i`: one a
//! daemon's process takes while its boot is under way and gives back as it ends, and one
//! whose owner ended without giving it back is the next waiter's at once (WAIT_ABANDONED),
//! so that no slot is lost with a daemon that died.

use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{HANDLE, WAIT_ABANDONED_0, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForMultipleObjects};

/// How often a boot waiting for a slot looks at its daemon's stop, in ms.
const LOOK_MS: u32 = 100;

/// The host's slots.
pub(super) struct HostBoots {
    slots: Vec<OwnedHandle>,
}

/// The slots a boot holds, given back as it drops: on the thread that took them, as a
/// mutex is its thread's (and so this is not Send).
pub(super) struct HostSlot<'a> {
    taken: Vec<HANDLE>,
    _of: PhantomData<(&'a HostBoots, *const ())>,
}

impl HostBoots {
    /// `count` slots named `name`, made or opened.
    pub(super) fn new(name: &str, count: usize) -> std::io::Result<HostBoots> {
        let slots = (0..count.max(1))
            .map(|i| {
                let wide: Vec<u16> = format!("Local\\{name}-{i}").encode_utf16().chain([0]).collect();
                // SAFETY: a NUL-terminated name, no security attributes, owned by no one yet.
                let made = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
                if made.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: a handle this process owns, closed once as it drops.
                Ok(unsafe { OwnedHandle::from_raw_handle(made) })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(HostBoots { slots })
    }

    /// One slot, or every one where `all` (a boot of more than one slot's vCPUs, which boots
    /// alone), waited for until taken; none once `stopping` is set.
    pub(super) fn take(&self, all: bool, stopping: &AtomicBool) -> Result<HostSlot<'_>, String> {
        let handles: Vec<HANDLE> = self.slots.iter().map(|h| h.as_raw_handle()).collect();
        let n = u32::try_from(handles.len()).map_err(|e| e.to_string())?;
        loop {
            if stopping.load(Ordering::SeqCst) {
                return Err("the daemon is shutting down".into());
            }
            // SAFETY: the slots' handles, which this value holds open.
            let r = unsafe { WaitForMultipleObjects(n, handles.as_ptr(), i32::from(all), LOOK_MS) };
            if r == WAIT_TIMEOUT {
                continue;
            }
            if r == WAIT_FAILED {
                return Err(format!(
                    "waiting for the host's boot slot: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // WAIT_OBJECT_0 + i, or WAIT_ABANDONED_0 + i for a slot whose owner ended
            // holding it: this thread's either way.
            let i = r
                .checked_sub(WAIT_ABANDONED_0)
                .filter(|i| *i < n)
                .or_else(|| r.checked_sub(WAIT_OBJECT_0).filter(|i| *i < n))
                .ok_or_else(|| format!("waiting for the host's boot slot: {r:#x}"))?;
            let taken = if all {
                handles
            } else {
                vec![*handles.get(i as usize).ok_or("a boot slot past the host's")?]
            };
            return Ok(HostSlot {
                taken,
                _of: PhantomData,
            });
        }
    }
}

impl Drop for HostSlot<'_> {
    fn drop(&mut self) {
        for h in &self.taken {
            // SAFETY: a mutex of the slots this borrows, which this thread owns.
            unsafe { ReleaseMutex(*h) };
        }
    }
}
