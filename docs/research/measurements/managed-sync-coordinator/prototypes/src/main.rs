//! Test artifact only: explicit pinned pthread lifecycle, no production wrapper.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("this mechanism probe is Darwin arm64 only");

use std::alloc::{Layout, alloc};
use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomPinned;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::time::Duration;

#[path = "../../../managed-borrowed/allocation.rs"]
mod allocation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    MutexInit,
    CondInit,
    Lock,
    Wait,
    Signal,
    Unlock,
    Create,
    Join,
    CondDestroy,
    MutexDestroy,
}
#[derive(Debug, Clone, Copy)]
enum Error {
    Allocation,
    Argument,
    Clock(Option<i32>),
    Arithmetic,
    Pthread { call: Call, code: i32 },
    LiveThread { call: Call, code: i32 },
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allocation => f.write_str("pinned storage allocation refused"),
            Self::Argument => f.write_str("expected mode and positive outer wait nanoseconds"),
            Self::Clock(code) => write!(f, "clock_gettime errno {code:?}"),
            Self::Arithmetic => f.write_str("absolute deadline overflow"),
            Self::Pthread { call, code } => write!(f, "{call:?} returned {code}"),
            Self::LiveThread { call, code } => {
                write!(f, "{call:?} returned {code}; worker retirement unknown")
            }
            Self::State => f.write_str("unexpected probe state"),
        }
    }
}
fn checked(call: Call, code: i32) -> Result<(), Error> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Pthread { call, code })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Empty,
    Mutex,
    Pair,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Starting,
    Waiting,
    Released,
    Done,
}

struct Storage {
    mutex: UnsafeCell<MaybeUninit<libc::pthread_mutex_t>>,
    cond: UnsafeCell<MaybeUninit<libc::pthread_cond_t>>,
    phase: Cell<Phase>,     // Main thread only, before create/after join.
    step: UnsafeCell<Step>, // Access only while this native mutex is held.
    worker_result: UnsafeCell<Result<(), Error>>, // Worker writes; main reads after join.
    deadline: libc::timespec,
    _pin: PhantomPinned,
}
impl Storage {
    fn mutex(&self) -> *mut libc::pthread_mutex_t {
        self.mutex.get().cast()
    }
    fn cond(&self) -> *mut libc::pthread_cond_t {
        self.cond.get().cast()
    }
}

fn storage(deadline: libc::timespec) -> Result<Pin<Box<Storage>>, Error> {
    // SAFETY: fixed nonzero layout; a successful Global allocation is transferred to
    // exactly one matching Box. No initialized native object has existed/moved yet.
    let pointer = unsafe { alloc(Layout::new::<Storage>()) }.cast::<Storage>();
    if pointer.is_null() {
        return Err(Error::Allocation);
    }
    // SAFETY: pointer owns aligned storage of precisely Storage's layout.
    unsafe {
        pointer.write(Storage {
            mutex: UnsafeCell::new(MaybeUninit::uninit()),
            cond: UnsafeCell::new(MaybeUninit::uninit()),
            phase: Cell::new(Phase::Empty),
            step: UnsafeCell::new(Step::Starting),
            worker_result: UnsafeCell::new(Ok(())),
            deadline,
            _pin: PhantomPinned,
        });
        Ok(Box::into_pin(Box::from_raw(pointer)))
    }
}

fn initialize(slot: Pin<&Storage>) -> Result<(), Error> {
    // SAFETY: storage was pinned before initialization; both opaque objects remain
    // there until destruction. NULL attrs request documented pthread defaults.
    checked(Call::MutexInit, unsafe {
        libc::pthread_mutex_init(slot.mutex(), ptr::null())
    })?;
    slot.phase.set(Phase::Mutex);
    // SAFETY: distinct pinned, as-yet-uninitialized condition variable storage.
    checked(Call::CondInit, unsafe {
        libc::pthread_cond_init(slot.cond(), ptr::null())
    })?;
    slot.phase.set(Phase::Pair);
    Ok(())
}

fn expired_wait(slot: &Storage) -> Result<(), Error> {
    // SAFETY: initialized pinned objects, exclusively locked by this thread.
    checked(Call::Lock, unsafe { libc::pthread_mutex_lock(slot.mutex()) })?;
    // Unix epoch is a valid expired absolute deadline, not a latency cutoff.
    let expired = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: owned mutex, valid absolute timespec; timed wait reacquires on timeout.
    let result = checked(Call::Wait, unsafe {
        libc::pthread_cond_timedwait(slot.cond(), slot.mutex(), &expired)
    });
    // SAFETY: the wait returned owning this same mutex, including ETIMEDOUT.
    checked(Call::Unlock, unsafe { libc::pthread_mutex_unlock(slot.mutex()) })?;
    result
}

fn worker_run(slot: &Storage) -> Result<(), Error> {
    // SAFETY: main keeps pinned objects alive until pthread_join, same mutex/condition.
    checked(Call::Lock, unsafe { libc::pthread_mutex_lock(slot.mutex()) })?;
    let result = (|| {
        // SAFETY: the mutex is held; only these two threads access step under it.
        unsafe {
            *slot.step.get() = Step::Waiting;
        }
        // SAFETY: initialized condition with the associated mutex held.
        checked(Call::Signal, unsafe { libc::pthread_cond_signal(slot.cond()) })?;
        // SAFETY: this thread holds the mutex at every predicate observation.
        while unsafe { *slot.step.get() } != Step::Released {
            // SAFETY: releases/reacquires same held mutex; fixed original deadline.
            checked(Call::Wait, unsafe {
                libc::pthread_cond_timedwait(slot.cond(), slot.mutex(), &slot.deadline)
            })?;
        }
        // SAFETY: predicate transition protected by the native mutex.
        unsafe {
            *slot.step.get() = Step::Done;
        }
        Ok(())
    })();
    // SAFETY: all wait returns own this mutex; no pthread cancellation is used.
    let unlock = checked(Call::Unlock, unsafe { libc::pthread_mutex_unlock(slot.mutex()) });
    result.and(unlock)
}
extern "C" fn worker(argument: *mut libc::c_void) -> *mut libc::c_void {
    // SAFETY: main retains the pinned allocation until this worker is joined. Only
    // UnsafeCell fields are mutated, under native mutex or post-join exclusivity.
    let slot = unsafe { &*argument.cast::<Storage>() };
    let result = worker_run(slot);
    // SAFETY: worker alone writes this; main reads only after successful pthread_join.
    unsafe {
        *slot.worker_result.get() = result;
    }
    ptr::null_mut()
}

fn exchange(slot: Pin<&Storage>) -> Result<(), Error> {
    // SAFETY: initialized pinned mutex; main is its sole current owner.
    checked(Call::Lock, unsafe { libc::pthread_mutex_lock(slot.mutex()) })?;
    let mut thread = MaybeUninit::<libc::pthread_t>::uninit();
    let argument = (slot.get_ref() as *const Storage).cast_mut().cast();
    // SAFETY: native thread gets only the pinned storage, retained until join.
    let created = checked(Call::Create, unsafe {
        libc::pthread_create(thread.as_mut_ptr(), ptr::null(), worker, argument)
    });
    if created.is_err() {
        // SAFETY: no worker was created and main still owns the mutex.
        checked(Call::Unlock, unsafe { libc::pthread_mutex_unlock(slot.mutex()) })?;
        return created;
    }
    // SAFETY: successful pthread_create initialized the thread handle (not a mutex).
    let thread = unsafe { thread.assume_init() };
    let result = (|| {
        // SAFETY: predicate access occurs only while holding the mutex.
        while unsafe { *slot.step.get() } == Step::Starting {
            // SAFETY: same associated mutex, one absolute outer liveness deadline.
            checked(Call::Wait, unsafe {
                libc::pthread_cond_timedwait(slot.cond(), slot.mutex(), &slot.deadline)
            })?;
        }
        Ok(())
    })();
    // SAFETY: main still owns the mutex; release also lets a timed-out trial finish.
    unsafe {
        *slot.step.get() = Step::Released;
    }
    // SAFETY: initialized condition, main holding its associated mutex.
    let signal = checked(Call::Signal, unsafe { libc::pthread_cond_signal(slot.cond()) });
    // SAFETY: main owns the mutex from lock or the completed timed wait.
    let unlocked = unsafe { libc::pthread_mutex_unlock(slot.mutex()) };
    if unlocked != 0 {
        return Err(Error::LiveThread {
            call: Call::Unlock,
            code: unlocked,
        });
    }
    // SAFETY: join this one created native worker exactly once, with no return payload.
    let joined = unsafe { libc::pthread_join(thread, ptr::null_mut()) };
    if joined != 0 {
        return Err(Error::LiveThread {
            call: Call::Join,
            code: joined,
        });
    }
    // SAFETY: worker completed; its result and protected final predicate are now stable.
    let worker = unsafe { *slot.worker_result.get() };
    result.and(signal).and(worker)?;
    // SAFETY: worker is joined and no concurrent predicate writer remains.
    if unsafe { *slot.step.get() } != Step::Done {
        return Err(Error::State);
    }
    Ok(())
}

fn destroy(slot: Pin<&Storage>) -> Result<(), Error> {
    if slot.phase.get() == Phase::Pair {
        // SAFETY: called only after successful join/no worker, no remaining waiter.
        checked(Call::CondDestroy, unsafe {
            libc::pthread_cond_destroy(slot.cond())
        })?;
        slot.phase.set(Phase::Mutex);
    }
    if slot.phase.get() == Phase::Mutex {
        // SAFETY: no owner/waiter remains; initialized mutex still at original address.
        checked(Call::MutexDestroy, unsafe {
            libc::pthread_mutex_destroy(slot.mutex())
        })?;
        slot.phase.set(Phase::Empty);
    }
    Ok(())
}

fn deadline(nanos: u64) -> Result<libc::timespec, Error> {
    if nanos == 0 {
        return Err(Error::Argument);
    }
    let mut now = MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: valid output for clock_gettime's CLOCK_REALTIME absolute timebase.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, now.as_mut_ptr()) } != 0 {
        return Err(Error::Clock(std::io::Error::last_os_error().raw_os_error()));
    }
    // SAFETY: successful clock_gettime initialized now.
    let mut now = unsafe { now.assume_init() };
    let duration = Duration::from_nanos(nanos);
    now.tv_sec = now
        .tv_sec
        .checked_add(duration.as_secs().try_into().map_err(|_| Error::Arithmetic)?)
        .ok_or(Error::Arithmetic)?;
    let extra = Duration::from_nanos(u64::try_from(now.tv_nsec).map_err(|_| Error::Arithmetic)?)
        .checked_add(Duration::from_nanos(u64::from(duration.subsec_nanos())))
        .ok_or(Error::Arithmetic)?;
    now.tv_sec = now
        .tv_sec
        .checked_add(extra.as_secs().try_into().map_err(|_| Error::Arithmetic)?)
        .ok_or(Error::Arithmetic)?;
    now.tv_nsec = extra.subsec_nanos().into();
    Ok(now)
}

fn run() -> Result<(), Error> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().ok_or(Error::Argument)?;
    let nanos = args
        .next()
        .and_then(|value| value.to_str().and_then(|s| s.parse::<u64>().ok()))
        .ok_or(Error::Argument)?;
    if args.next().is_some() {
        return Err(Error::Argument);
    }
    let deadline = deadline(nanos)?;
    if mode == "allocation-refused" {
        return match allocation::fail_after(0, || storage(deadline)) {
            Err(Error::Allocation) => {
                println!("pinned_storage=Allocation native_init_not_entered=true");
                Ok(())
            }
            _ => Err(Error::State),
        };
    }
    if mode != "native-under-rust-refusal" {
        return Err(Error::Argument);
    }
    let slot = storage(deadline)?;
    let address = slot.as_ref().get_ref() as *const Storage;
    let (native, cleanup, timeout) = allocation::fail_after(0, || {
        let mut timeout = None;
        let native = (|| {
            initialize(slot.as_ref())?;
            match expired_wait(&slot) {
                Err(
                    error @ Error::Pthread {
                        call: Call::Wait,
                        code,
                    },
                ) if code == libc::ETIMEDOUT => timeout = Some(error),
                _ => return Err(Error::State),
            }
            exchange(slot.as_ref())
        })();
        let cleanup = if matches!(native, Err(Error::LiveThread { .. })) {
            Err(Error::State)
        } else {
            destroy(slot.as_ref())
        };
        (native, cleanup, timeout)
    });
    let retained = cleanup.is_err();
    println!(
        "native={native:?} expired_wait_typed={timeout:?} cleanup={cleanup:?} retained_unknown={retained} pinned_address={address:p}"
    );
    if retained {
        std::mem::forget(slot);
    } // Never free possibly-live/undestroyed pthread storage.
    native.and(cleanup)
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("probe_error={error}");
            std::process::ExitCode::FAILURE
        }
    }
}
