//! Acquired Linux storage, not a management grant or a future allocation promise.
//!
//! Each region owns an initialized private RW/NX mapping and ordinary mlock of
//! its entire page-rounded extent. DONTFORK prevents a later child from adding
//! COW obligations to these pages. Original owner/ancestor admission and induced
//! kernel/transport resources remain the caller's independent responsibilities.
//! See docs/research/measurements/managed-native-regions-v1/DECISION.md.

use std::io;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Map,
    ExcludeChild,
    Lock,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Length,
    PageSize {
        actual: libc::c_long,
    },
    NullAddress,
    WrongProcess,
    Native {
        operation: Operation,
        kind: io::ErrorKind,
        errno: Option<i32>,
    },
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "native storage: {self:?}")
    }
}

impl std::error::Error for Failure {}

fn native(operation: Operation) -> Failure {
    let error = io::Error::last_os_error();
    Failure::Native {
        operation,
        kind: error.kind(),
        errno: error.raw_os_error(),
    }
}

/// Sizes of the actual held representation; none is an entitlement or a limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub usable: NonZeroUsize,
    pub mapped: NonZeroUsize,
    pub page: NonZeroUsize,
}

impl Extent {
    fn checked(usable: NonZeroUsize, page: NonZeroUsize) -> Result<Self, Failure> {
        let remainder = usable.get() % page.get();
        let extra = if remainder == 0 {
            0
        } else {
            page.get() - remainder
        };
        let mapped = usable
            .get()
            .checked_add(extra)
            .filter(|bytes| isize::try_from(*bytes).is_ok())
            .and_then(NonZeroUsize::new)
            .ok_or(Failure::Length)?;
        Ok(Self {
            usable,
            mapped,
            page,
        })
    }
}

enum State {
    Held {
        address: *mut libc::c_void,
        process: libc::pid_t,
    },
    Released,
}

struct Mapping {
    state: State,
    extent: Extent,
}

impl Mapping {
    fn address(&self) -> Result<*mut libc::c_void, Failure> {
        match self.state {
            State::Held { address, process } => {
                // SAFETY: getpid has no pointer argument or ownership side effect.
                if unsafe { libc::getpid() } == process {
                    Ok(address)
                } else {
                    Err(Failure::WrongProcess)
                }
            }
            State::Released => Err(Failure::Length),
        }
    }

    fn release(&mut self) -> Result<(), Failure> {
        if matches!(self.state, State::Released) {
            return Ok(());
        }
        let address = self.address()?;
        // SAFETY: this is the exact still-owned mmap range; no slice survives
        // the consuming public release. Successful unmap also removes its locks.
        if unsafe { libc::munmap(address, self.extent.mapped.get()) } != 0 {
            return Err(native(Operation::Release));
        }
        self.state = State::Released;
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // Explicit release returns any failed cleanup to the real owner. Drop
        // is only best effort, never a capacity-return or kernel-return receipt.
        // address() also prevents a copied post-fork wrapper from unmapping an
        // unrelated child allocation where the excluded region used to be.
        let _ = self.release();
    }
}

/// An unready or release-failed mapping that still needs explicit cleanup.
pub struct Retained {
    mapping: Mapping,
    failure: Failure,
}

impl Retained {
    pub fn extent(&self) -> Extent {
        self.mapping.extent
    }
    pub fn failure(&self) -> Failure {
        self.failure
    }

    /// Retry only at the actual owner's chosen cleanup event; no timer or loop.
    pub fn release(mut self) -> Result<(), Self> {
        match self.mapping.release() {
            Ok(()) => Ok(()),
            Err(failure) => {
                self.failure = failure;
                Err(self)
            }
        }
    }
}

impl std::fmt::Debug for Retained {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("RetainedRegion")
            .field("extent", &self.mapping.extent)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum Rejected {
    Refused(Failure),
    Retained { cause: Failure, storage: Retained },
}

impl Rejected {
    fn cleanup(mut mapping: Mapping, cause: Failure) -> Self {
        match mapping.release() {
            Ok(()) => Self::Refused(cause),
            Err(failure) => Self::Retained {
                cause,
                storage: Retained { mapping, failure },
            },
        }
    }
}

/// Initialized, resident private bytes. Not Clone; no descriptor or raw owner
/// token can manufacture it. Kernel/ancestor guarantees are not implied.
pub struct Region {
    mapping: Mapping,
}

impl std::fmt::Debug for Region {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("LockedRegion")
            .field("extent", &self.mapping.extent)
            .finish_non_exhaustive()
    }
}

impl Region {
    /// Caller supplies the exact required representation length after separate
    /// owner admission. This changes no limit, capability or global VM setting.
    pub fn acquire_zeroed(bytes: NonZeroUsize) -> Result<Self, Rejected> {
        // SAFETY: sysconf queries the running kernel/libc's actual page size.
        let actual = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = usize::try_from(actual)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or(Rejected::Refused(Failure::PageSize { actual }))?;
        let extent = Extent::checked(bytes, page).map_err(Rejected::Refused)?;
        // SAFETY: positive checked extent, no fixed address, file, executable
        // permission or other existing mapping is changed.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                extent.mapped.get(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(Rejected::Refused(native(Operation::Map)));
        }
        // SAFETY: getpid has no pointer argument or ownership side effect.
        let process = unsafe { libc::getpid() };
        let mapping = Mapping {
            state: State::Held {
                address: raw,
                process,
            },
            extent,
        };
        let Some(address) = NonNull::new(raw.cast::<u8>()) else {
            return Err(Rejected::cleanup(mapping, Failure::NullAddress));
        };
        // SAFETY: the complete live mapping is owned here. This is applied before
        // locks/publication so future fork cannot create private-page COW debt.
        if unsafe { libc::madvise(raw, extent.mapped.get(), libc::MADV_DONTFORK) } != 0 {
            return Err(Rejected::cleanup(mapping, native(Operation::ExcludeChild)));
        }
        // SAFETY: exact accessible RW range. Ordinary mlock (not ONFAULT or
        // MAP_LOCKED) must populate the complete range or report failure.
        if unsafe { libc::mlock(raw, extent.mapped.get()) } != 0 {
            return Err(Rejected::cleanup(mapping, native(Operation::Lock)));
        }
        // SAFETY: all mapped bytes belong to this initialized writable region;
        // no borrow or child can observe the representation before completion.
        unsafe { std::ptr::write_bytes(address.as_ptr(), 0, extent.mapped.get()) };
        Ok(Self { mapping })
    }

    pub fn extent(&self) -> Extent {
        self.mapping.extent
    }

    pub fn bytes(&self) -> Result<&[u8], Failure> {
        let address =
            NonNull::new(self.mapping.address()?.cast::<u8>()).ok_or(Failure::NullAddress)?;
        // SAFETY: exact initialized usable extent, checked <=isize::MAX;
        // the shared borrow keeps the uniquely owned mapping alive.
        Ok(unsafe {
            std::slice::from_raw_parts(address.as_ptr(), self.mapping.extent.usable.get())
        })
    }

    pub fn bytes_mut(&mut self) -> Result<&mut [u8], Failure> {
        let address =
            NonNull::new(self.mapping.address()?.cast::<u8>()).ok_or(Failure::NullAddress)?;
        // SAFETY: exclusive borrow of the same live initialized usable range.
        Ok(unsafe {
            std::slice::from_raw_parts_mut(address.as_ptr(), self.mapping.extent.usable.get())
        })
    }

    /// A successful result witnesses this exact mapping's unmap, not release of
    /// other references, slots, memfds, SCM queues or an operation's lifetime.
    pub fn release(mut self) -> Result<(), Retained> {
        match self.mapping.release() {
            Ok(()) => Ok(()),
            Err(failure) => Err(Retained {
                mapping: self.mapping,
                failure,
            }),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests;
