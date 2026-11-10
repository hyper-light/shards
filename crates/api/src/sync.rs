//! State/Tail synchronization. Darwin requires fallible stable native storage;
//! the other supported targets retain std's existing inline futex implementation.
//! Decision and native cold red: managed-cold-sync/current/BINDING.md.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Storage,
    MutexAttributes,
    MutexType,
    MutexInit,
    MutexLock,
    MutexUnlock,
    MutexDestroy,
    CondvarInit,
    CondvarWait,
    CondvarNotify,
    CondvarDestroy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    pub operation: Operation,
    /// None denotes allocation refusal; native failures retain their actual code.
    pub code: Option<i32>,
    pub cleanup_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitStatus {
    NotifiedOrSpurious,
    TimedOut,
}

#[cfg(target_vendor = "apple")]
impl Failure {
    fn native(operation: Operation, code: i32) -> Self {
        Self {
            operation,
            code: Some(code),
            cleanup_code: None,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "guest synchronization {:?}", self.operation)?;
        if let Some(code) = self.code {
            write!(out, ": native code {code}")?;
        }
        if let Some(code) = self.cleanup_code {
            write!(out, "; cleanup code {code}")?;
        }
        Ok(())
    }
}
impl std::error::Error for Failure {}

pub(crate) use implementation::{Condvar, Guard, Mutex};

#[cfg(target_vendor = "apple")]
mod implementation {
    use super::{Failure, Operation, WaitStatus};
    use std::alloc::{Layout, alloc, dealloc};
    use std::cell::UnsafeCell;
    use std::marker::PhantomData;
    use std::mem::{ManuallyDrop, MaybeUninit};
    use std::ops::{Deref, DerefMut};
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration;

    struct Inner<T> {
        raw: UnsafeCell<libc::pthread_mutex_t>,
        value: UnsafeCell<T>,
    }

    pub(crate) struct Mutex<T> {
        inner: ManuallyDrop<Box<Inner<T>>>,
        // Native failure is permanent for this object. In particular, a reaper may
        // clean its Child after lock refusal without a later caller entering stale
        // State. These are synchronization errors, not guest lifecycle mirrors.
        lock_failure: AtomicI32,
        unlock_failure: AtomicI32,
    }

    impl<T> std::fmt::Debug for Mutex<T> {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.write_str("Mutex { .. }")
        }
    }

    // SAFETY: value is accessed only under the native exclusive guard; moving this
    // wrapper does not move the initialized native mutex allocation.
    unsafe impl<T: Send> Send for Mutex<T> {}
    // SAFETY: concurrent access to T requires the same native exclusive mutex.
    unsafe impl<T: Send> Sync for Mutex<T> {}

    impl<T> Mutex<T> {
        pub(crate) fn new(value: T) -> Result<Self, (T, Failure)> {
            let layout = Layout::new::<Inner<T>>();
            // SAFETY: nonzero correctly aligned layout (native mutex is nonzero).
            let memory = unsafe { alloc(layout) }.cast::<Inner<T>>();
            let Some(memory) = NonNull::new(memory) else {
                return Err((
                    value,
                    Failure {
                        operation: Operation::Storage,
                        code: None,
                        cleanup_code: None,
                    },
                ));
            };
            // SAFETY: only the allocated field's address is formed; no uninitialized
            // value is read. pthread initialization occurs at the final stable address.
            let raw = unsafe { ptr::addr_of_mut!((*memory.as_ptr()).raw).cast::<libc::pthread_mutex_t>() };
            // SAFETY: initialize every opaque Rust byte before native init, as Rust
            // PAL does. Native init need not write every byte of an opaque C object.
            unsafe { raw.write(libc::PTHREAD_MUTEX_INITIALIZER) };
            let mut attributes = MaybeUninit::<libc::pthread_mutexattr_t>::uninit();
            // SAFETY: writable attribute storage supplied for initialization.
            let attr = unsafe { libc::pthread_mutexattr_init(attributes.as_mut_ptr()) };
            let initialized = if attr == 0 {
                // SAFETY: attributes were initialized above. NORMAL prevents the
                // DEFAULT recursive-lock UB described by the pinned Rust PAL source.
                let kind = unsafe {
                    libc::pthread_mutexattr_settype(attributes.as_mut_ptr(), libc::PTHREAD_MUTEX_NORMAL)
                };
                let init = if kind == 0 {
                    // SAFETY: stable allocated native storage and live attributes.
                    unsafe { libc::pthread_mutex_init(raw, attributes.as_ptr()) }
                } else {
                    kind
                };
                // SAFETY: attributes are initialized and no longer needed by init.
                let cleaned = unsafe { libc::pthread_mutexattr_destroy(attributes.as_mut_ptr()) };
                if init != 0 {
                    let mut failure = Failure::native(
                        if kind == 0 {
                            Operation::MutexInit
                        } else {
                            Operation::MutexType
                        },
                        init,
                    );
                    failure.cleanup_code = (cleaned != 0).then_some(cleaned);
                    Err(failure)
                } else if cleaned != 0 {
                    // SAFETY: successfully initialized but unpublished, never locked.
                    let destroyed = unsafe { libc::pthread_mutex_destroy(raw) };
                    let mut failure = Failure::native(Operation::MutexAttributes, cleaned);
                    failure.cleanup_code = (destroyed != 0).then_some(destroyed);
                    if destroyed != 0 {
                        // Preserve the live native allocation rather than free an
                        // object whose destruction failed. No T was moved into it.
                        return Err((value, failure));
                    }
                    Err(failure)
                } else {
                    Ok(())
                }
            } else {
                Err(Failure::native(Operation::MutexAttributes, attr))
            };
            if let Err(failure) = initialized {
                // SAFETY: native init failed or its unpublished object was destroyed;
                // T remains owned by the caller and was never written into memory.
                unsafe { dealloc(memory.as_ptr().cast(), layout) };
                return Err((value, failure));
            }
            // SAFETY: mutex is initialized at its final address. Initialize the last
            // field before constructing Box, with exactly the allocator/layout used.
            unsafe {
                ptr::addr_of_mut!((*memory.as_ptr()).value).write(UnsafeCell::new(value));
                Ok(Self {
                    inner: ManuallyDrop::new(Box::from_raw(memory.as_ptr())),
                    lock_failure: AtomicI32::new(0),
                    unlock_failure: AtomicI32::new(0),
                })
            }
        }

        pub(crate) fn lock(&self) -> Result<Guard<'_, T>, Failure> {
            self.checked()?;
            // SAFETY: initialized stable mutex retained by self and guarded lifetime.
            let code = unsafe { libc::pthread_mutex_lock(self.inner.raw.get()) };
            if code != 0 {
                self.lock_failure.store(code, Ordering::Release);
                return Err(Failure::native(Operation::MutexLock, code));
            }
            if let Err(failure) = self.checked() {
                // SAFETY: this thread acquired the native lock above, but another
                // acquisition reported a failure. Never expose the protected value.
                self.unlocked(unsafe { libc::pthread_mutex_unlock(self.inner.raw.get()) })?;
                return Err(failure);
            }
            Ok(Guard {
                owner: self,
                held: true,
                not_send: PhantomData,
            })
        }

        fn checked(&self) -> Result<(), Failure> {
            for (code, operation) in [
                (self.lock_failure.load(Ordering::Acquire), Operation::MutexLock),
                (
                    self.unlock_failure.load(Ordering::Acquire),
                    Operation::MutexUnlock,
                ),
            ] {
                if code != 0 {
                    return Err(Failure::native(operation, code));
                }
            }
            Ok(())
        }

        fn unlocked(&self, code: i32) -> Result<(), Failure> {
            if code == 0 {
                Ok(())
            } else {
                self.unlock_failure.store(code, Ordering::Release);
                Err(Failure::native(Operation::MutexUnlock, code))
            }
        }
    }

    impl<T> Drop for Mutex<T> {
        fn drop(&mut self) {
            // SAFETY: no unforgotten guard outlives self. A forgotten guard may leave
            // it locked; in that case preserve the allocation, as std also does.
            let code = unsafe { libc::pthread_mutex_destroy(self.inner.raw.get()) };
            if code == 0 {
                // SAFETY: native object is quiescent and successfully destroyed.
                unsafe { ManuallyDrop::drop(&mut self.inner) };
            }
        }
    }

    pub(crate) struct Guard<'a, T> {
        owner: &'a Mutex<T>,
        /// Owns only this native acquisition, not a duplicate guest lifecycle fact.
        held: bool,
        not_send: PhantomData<*mut ()>,
    }

    impl<T> Guard<'_, T> {
        pub(crate) fn release(mut self) -> Result<(), Failure> {
            // SAFETY: Guard is not Send and only constructed after successful lock.
            let code = unsafe { libc::pthread_mutex_unlock(self.owner.inner.raw.get()) };
            self.held = false;
            self.owner.unlocked(code)
        }
    }

    impl<T> Deref for Guard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: this guard owns the native mutex's exclusive acquisition.
            unsafe { &*self.owner.inner.value.get() }
        }
    }

    impl<T> DerefMut for Guard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: mutable guard access and native acquisition exclude aliases.
            unsafe { &mut *self.owner.inner.value.get() }
        }
    }

    impl<T> Drop for Guard<'_, T> {
        fn drop(&mut self) {
            if self.held {
                // SAFETY: lifetime/thread ownership as in release. Explicit release
                // reports native errors; this unwind/forgotten-release path is best
                // effort and does not claim native cleanup/error propagation proof.
                let _ = self
                    .owner
                    .unlocked(unsafe { libc::pthread_mutex_unlock(self.owner.inner.raw.get()) });
            }
        }
    }

    pub(crate) struct Condvar {
        raw: ManuallyDrop<Box<UnsafeCell<libc::pthread_cond_t>>>,
    }

    // SAFETY: initialized stable condition variable supports concurrent wait/notify.
    unsafe impl Send for Condvar {}
    // SAFETY: native calls synchronize; Rust never independently mutates its bytes.
    unsafe impl Sync for Condvar {}

    impl Condvar {
        pub(crate) fn new() -> Result<Self, Failure> {
            let layout = Layout::new::<UnsafeCell<libc::pthread_cond_t>>();
            // SAFETY: nonzero aligned native object storage.
            let memory = unsafe { alloc(layout) }.cast::<UnsafeCell<libc::pthread_cond_t>>();
            let Some(memory) = NonNull::new(memory) else {
                return Err(Failure {
                    operation: Operation::Storage,
                    code: None,
                    cleanup_code: None,
                });
            };
            // SAFETY: all opaque bytes have a valid initialized Rust representation
            // before pthread_cond_init writes its implementation-specific fields.
            unsafe {
                memory
                    .as_ptr()
                    .write(UnsafeCell::new(libc::PTHREAD_COND_INITIALIZER))
            };
            // SAFETY: stable writable native storage; default native attributes.
            let code = unsafe { libc::pthread_cond_init(memory.as_ptr().cast(), ptr::null()) };
            if code != 0 {
                // SAFETY: failed init did not construct a native object.
                unsafe { dealloc(memory.as_ptr().cast(), layout) };
                return Err(Failure::native(Operation::CondvarInit, code));
            }
            Ok(Self {
                // SAFETY: initialized same-allocation native object, no relocation.
                raw: ManuallyDrop::new(unsafe { Box::from_raw(memory.as_ptr()) }),
            })
        }

        pub(crate) fn notify_all(&self) -> Result<(), Failure> {
            // SAFETY: initialized condition variable retained by self.
            let code = unsafe { libc::pthread_cond_broadcast(self.raw.get()) };
            if code == 0 {
                Ok(())
            } else {
                Err(Failure::native(Operation::CondvarNotify, code))
            }
        }

        /// Private caller pairs this CV with the same mutex for all overlapping waits.
        /// Returns its still-owned guard along with the native result; no error creates
        /// a guard for an acquisition that never succeeded.
        /// # Safety
        /// The private caller uses this CV with one mutex for all overlapping waits.
        pub(crate) unsafe fn wait<'a, T>(
            &self,
            guard: Guard<'a, T>,
            timeout: Option<Duration>,
        ) -> (Guard<'a, T>, Result<WaitStatus, Failure>) {
            // Darwin's relative timed wait is monotonic. Cap only to its u64
            // nanosecond representation, as pinned Rust PAL does; caller retains its
            // original deadline and predicate loop, not a reset timeout policy.
            let code = unsafe {
                // SAFETY: the guard holds the native mutex; both objects are pinned
                // by allocations and private caller pairing is a required precondition.
                if let Some(duration) = timeout {
                    let duration = duration.min(Duration::from_nanos(u64::MAX));
                    let timespec = libc::timespec {
                        tv_sec: duration.as_secs() as libc::time_t,
                        tv_nsec: duration.subsec_nanos().into(),
                    };
                    libc::pthread_cond_timedwait_relative_np(
                        self.raw.get(),
                        guard.owner.inner.raw.get(),
                        &timespec,
                    )
                } else {
                    libc::pthread_cond_wait(self.raw.get(), guard.owner.inner.raw.get())
                }
            };
            let result = if code == 0 {
                Ok(WaitStatus::NotifiedOrSpurious)
            } else if code == libc::ETIMEDOUT {
                Ok(WaitStatus::TimedOut)
            } else {
                Err(Failure::native(Operation::CondvarWait, code))
            };
            (guard, result)
        }
    }

    impl Drop for Condvar {
        fn drop(&mut self) {
            // SAFETY: native object is initialized. Preserve allocation if native
            // destruction refuses; no leak-free claim follows from this destructor.
            let code = unsafe { libc::pthread_cond_destroy(self.raw.get()) };
            if code == 0 {
                // SAFETY: successful native destruction proves no active waiters.
                unsafe { ManuallyDrop::drop(&mut self.raw) };
            }
        }
    }

    impl std::fmt::Debug for Condvar {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.write_str("Condvar { .. }")
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
mod implementation {
    use super::{Failure, WaitStatus};
    use std::ops::{Deref, DerefMut};
    use std::sync::{self, PoisonError};
    use std::time::Duration;

    #[derive(Debug)]
    pub(crate) struct Mutex<T>(sync::Mutex<T>);
    impl<T> Mutex<T> {
        pub(crate) fn new(value: T) -> Result<Self, (T, Failure)> {
            Ok(Self(sync::Mutex::new(value)))
        }
        pub(crate) fn lock(&self) -> Result<Guard<'_, T>, Failure> {
            Ok(Guard(self.0.lock().unwrap_or_else(PoisonError::into_inner)))
        }
    }
    pub(crate) struct Guard<'a, T>(sync::MutexGuard<'a, T>);
    impl<T> Guard<'_, T> {
        pub(crate) fn release(self) -> Result<(), Failure> {
            drop(self);
            Ok(())
        }
    }
    impl<T> Deref for Guard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }
    impl<T> DerefMut for Guard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.0
        }
    }
    #[derive(Debug)]
    pub(crate) struct Condvar(sync::Condvar);
    impl Condvar {
        pub(crate) fn new() -> Result<Self, Failure> {
            Ok(Self(sync::Condvar::new()))
        }
        pub(crate) fn notify_all(&self) -> Result<(), Failure> {
            self.0.notify_all();
            Ok(())
        }
        /// # Safety
        /// All overlapping waits on this CV use the same mutex, as on Darwin.
        pub(crate) unsafe fn wait<'a, T>(
            &self,
            guard: Guard<'a, T>,
            timeout: Option<Duration>,
        ) -> (Guard<'a, T>, Result<WaitStatus, Failure>) {
            match timeout {
                Some(duration) => {
                    let (guard, status) = self
                        .0
                        .wait_timeout(guard.0, duration)
                        .unwrap_or_else(PoisonError::into_inner);
                    (
                        Guard(guard),
                        Ok(if status.timed_out() {
                            WaitStatus::TimedOut
                        } else {
                            WaitStatus::NotifiedOrSpurious
                        }),
                    )
                }
                None => (
                    Guard(self.0.wait(guard.0).unwrap_or_else(PoisonError::into_inner)),
                    Ok(WaitStatus::NotifiedOrSpurious),
                ),
            }
        }
    }
}
