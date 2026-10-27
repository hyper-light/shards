//! shards' own streams on Windows (docs/design/architecture.md D195): what its processes take
//! a Unix socket pair for elsewhere. Windows has no socketpair(): AF_UNIX makes a pair only
//! through a path in the filesystem, at 0.6 to 0.7 ms (PM M199), and loses what its peer had
//! not read where a writer's exit closes the last reference to it. A named pipe's pair is
//! made under a fresh name, as its name's only instance, remote clients refused, at 10 to 14
//! µs; once connected it refuses a second client; and its reader reads everything written
//! before its writer's end, its close or its exit (M340).
//!
//! Every end is opened for overlapped calls, each read or write a call of its own, waited
//! for on an event of its thread's ([`call`]): Windows takes one synchronous call on a file
//! object at a time, so that a thread waiting to read would hold up another's write, as
//! D140 found of sockets made so (M207).
//!
//! A [`Stream`]'s clones share its end, as a socket's descriptors share the socket:
//! `shutdown(Read)` ends every read of it, those waiting too, and `shutdown(Write)` every
//! write; ending both ([`crate::end`]) closes the end too, once no call of it is under way,
//! which its peer then reads as its end after all that was written. A pipe has no
//! half-close: its writes ended alone, its peer reads on.
//!
//! A message's handles go to its receiver's process ([`send`]). Between ends of one process
//! they go through the pair's own queue, never as values in the pipe: a value read from a
//! pipe is a handle of this process's only where this process's spawner put it there.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::io::{AsHandle, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING,
    ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, STATUS_PENDING, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT, PeekNamedPipe,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcessToken, ResetEvent, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};

use crate::{MAX_FDS, MAX_PAYLOAD};

mod poll;
pub use poll::{ExitWatch, Poller};

/// What each direction of a pipe of shards' holds before a writer waits: M340's larger
/// buffer, at which its throughput was the more even (64 KiB moved 3.3 to 13.4 GB/s, 1 MiB
/// 4.1 to 15.1). The pipe takes these as quotas, not allocations (CreateNamedPipeW).
pub(crate) const BUFFER: u32 = 1 << 20;

/// `code`, a Win32 error, as an `io::Error`.
pub(crate) fn error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}

/// Whether `e` says a pipe's other end has gone: its read is the end of the stream.
pub(crate) fn ended(e: &io::Error) -> bool {
    e.raw_os_error()
        .and_then(|c| u32::try_from(c).ok())
        .is_some_and(|c| c == ERROR_BROKEN_PIPE || c == ERROR_PIPE_NOT_CONNECTED || c == ERROR_NO_DATA)
}

/// An event object, manual-reset: set, it stays so until reset. What a Windows process of
/// shards' wakes another thread with, where Unix writes a byte to a pipe.
#[derive(Debug)]
pub struct Event(OwnedHandle);

impl Event {
    pub fn new() -> io::Result<Event> {
        // SAFETY: an unnamed manual-reset event, not set, with the default security.
        let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the event just made, which nothing else owns.
        Ok(Event(unsafe { OwnedHandle::from_raw_handle(raw) }))
    }

    pub fn set(&self) {
        // SAFETY: an event this value holds.
        unsafe { SetEvent(self.raw()) };
    }

    pub fn reset(&self) {
        // SAFETY: an event this value holds.
        unsafe { ResetEvent(self.raw()) };
    }

    /// Whether it is set, without waiting.
    pub fn is_set(&self) -> bool {
        // SAFETY: an event this value holds, not waited for.
        unsafe { WaitForSingleObject(self.raw(), 0) == WAIT_OBJECT_0 }
    }

    /// Another handle to the same event.
    pub fn try_clone(&self) -> io::Result<Event> {
        Ok(Event(self.0.try_clone()?))
    }

    pub fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }
}

impl AsHandle for Event {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

thread_local! {
    /// The event this thread's overlapped calls are waited for on ([`call`]), made as first
    /// needed.
    static CALLS: std::cell::RefCell<Option<Event>> = const { std::cell::RefCell::new(None) };
}

/// How an overlapped call ended.
#[derive(Debug)]
pub(crate) enum Called {
    /// It finished: the bytes it moved, or its error.
    Done(io::Result<usize>),
    /// What it was waited for beside came first, and it was cancelled with nothing moved.
    Cancelled,
    /// Its deadline came first, and it was cancelled with nothing moved.
    Late,
}

/// Makes one overlapped call on `handle`, begun by `start` with an OVERLAPPED it must pass
/// on, and waits for it to finish, for one of `cancels` to be set, or for `deadline`: the
/// call goes first where both have come. A call cancelled after it finished is `Done`, what
/// it moved moved. No call is left under way: its OVERLAPPED and buffer are its caller's,
/// on its stack, so this waits out a cancel too.
pub(crate) fn call(
    handle: HANDLE,
    cancels: &[&Event],
    deadline: Option<Instant>,
    start: impl FnOnce(*mut OVERLAPPED) -> i32,
) -> Called {
    let mut start = Some(start);
    let mut run = |event: &Event| {
        event.reset();
        let mut ov = OVERLAPPED {
            hEvent: event.raw(),
            ..OVERLAPPED::default()
        };
        let Some(start) = start.take() else {
            return Called::Done(Err(io::Error::other("a call made twice")));
        };
        if start(&raw mut ov) == 0 {
            // SAFETY: no arguments.
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                return Called::Done(Err(error(code)));
            }
        }
        let waited: Vec<HANDLE> = std::iter::once(event.raw())
            .chain(cancels.iter().map(|c| c.raw()))
            .collect();
        let count = u32::try_from(waited.len()).unwrap_or(1);
        loop {
            let ms = deadline.map_or(INFINITE, |d| {
                // Rounded up: a wait of 0 ms would end at once, and spin.
                let left = d.saturating_duration_since(Instant::now());
                u32::try_from(left.as_micros().div_ceil(1000))
                    .unwrap_or(INFINITE - 1)
                    .min(INFINITE - 1)
            });
            // SAFETY: `count` handles this call holds: its event, and its cancels'.
            let woke = unsafe { WaitForMultipleObjects(count, waited.as_ptr(), 0, ms) };
            match woke {
                WAIT_OBJECT_0 => return Called::Done(finished(handle, &ov, false)),
                w if w > WAIT_OBJECT_0 && w < WAIT_OBJECT_0 + count => {
                    return cancelled(handle, &ov, Called::Cancelled);
                }
                WAIT_TIMEOUT if deadline.is_some_and(|d| Instant::now() >= d) => {
                    return cancelled(handle, &ov, Called::Late);
                }
                WAIT_TIMEOUT => {}
                _ => {
                    let e = io::Error::last_os_error();
                    let _ = cancelled(handle, &ov, Called::Cancelled);
                    return Called::Done(Err(if woke == WAIT_FAILED {
                        e
                    } else {
                        io::Error::other(format!("a wait answered {woke}"))
                    }));
                }
            }
        }
    };
    // This thread's event, where it can have one: else one made for this call.
    let kept = CALLS.try_with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return None;
        };
        if slot.is_none() {
            *slot = Event::new().ok();
        }
        slot.as_ref().map(&mut run)
    });
    match kept {
        Ok(Some(called)) => called,
        _ => match Event::new() {
            Ok(event) => run(&event),
            Err(e) => Called::Done(Err(e)),
        },
    }
}

/// What the call on `ov` came to, waiting for it with `wait`.
fn finished(handle: HANDLE, ov: &OVERLAPPED, wait: bool) -> io::Result<usize> {
    let mut moved = 0u32;
    // SAFETY: the OVERLAPPED of a call made on `handle`, which lives until this returns.
    if unsafe { GetOverlappedResult(handle, ov, &raw mut moved, i32::from(wait)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(usize::try_from(moved).unwrap_or(usize::MAX))
}

/// Cancels the call on `ov` and waits for it: `why` where it moved nothing, what it moved
/// where it finished first.
fn cancelled(handle: HANDLE, ov: &OVERLAPPED, why: Called) -> Called {
    // SAFETY: the OVERLAPPED of a call made on `handle`; one finished already is no error.
    unsafe { CancelIoEx(handle, ov) };
    match finished(handle, ov, true) {
        Err(e) if e.raw_os_error() == i32::try_from(ERROR_OPERATION_ABORTED).ok() => why,
        done => Called::Done(done),
    }
}

/// Reads into `buf` from `handle` with [`call`].
pub(crate) fn read_into(
    handle: HANDLE,
    buf: &mut [u8],
    cancels: &[&Event],
    deadline: Option<Instant>,
) -> Called {
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let at = buf.as_mut_ptr();
    // SAFETY: a buffer of `len` bytes, which `call` keeps until the call has finished.
    call(handle, cancels, deadline, |ov| unsafe {
        ReadFile(handle, at, len, std::ptr::null_mut(), ov)
    })
}

/// Writes from `buf` to `handle` with [`call`].
pub(crate) fn write_from(
    handle: HANDLE,
    buf: &[u8],
    cancels: &[&Event],
    deadline: Option<Instant>,
) -> Called {
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let at = buf.as_ptr();
    // SAFETY: as `read_into`'s.
    call(handle, cancels, deadline, |ov| unsafe {
        WriteFile(handle, at, len, std::ptr::null_mut(), ov)
    })
}

/// Waits for a client to open the instance `server`, or for `cancel`: `Ok(true)` once one
/// has, a client gone again since among them; `Ok(false)` for the cancel.
pub(crate) fn connected(server: HANDLE, cancels: &[&Event]) -> io::Result<bool> {
    // SAFETY: a pipe instance's server end, and the OVERLAPPED `call` keeps.
    match call(server, cancels, None, |ov| unsafe {
        ConnectNamedPipe(server, ov)
    }) {
        Called::Done(Ok(_)) => Ok(true),
        Called::Done(Err(e))
            if e.raw_os_error()
                .and_then(|c| u32::try_from(c).ok())
                .is_some_and(|c| c == ERROR_PIPE_CONNECTED || c == ERROR_NO_DATA) =>
        {
            Ok(true)
        }
        Called::Done(Err(e)) => Err(e),
        Called::Cancelled | Called::Late => Ok(false),
    }
}

/// What a pipe's `handle` has to read now, without taking it: its bytes, its first copied
/// into `first` where there is room; `None` once its other end has gone.
pub(crate) fn available(handle: HANDLE, first: Option<&mut u8>) -> io::Result<Option<u32>> {
    let (mut read, mut avail) = (0u32, 0u32);
    let (at, len) = match first {
        Some(byte) => (std::ptr::from_mut(byte).cast(), 1),
        None => (std::ptr::null_mut(), 0),
    };
    // SAFETY: `len` bytes at `at`, or none.
    if unsafe {
        PeekNamedPipe(
            handle,
            at,
            len,
            &raw mut read,
            &raw mut avail,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let e = io::Error::last_os_error();
        return if ended(&e) { Ok(None) } else { Err(e) };
    }
    Ok(Some(avail))
}

/// A pipe's attributes for this user alone: its DACL grants this process's user, and no
/// one else, not an AppContainer of that user's (a VM process's) nor another user; the
/// default DACL would give Everyone read (CreateNamedPipeW). Made once a process.
pub(crate) fn attributes() -> io::Result<SECURITY_ATTRIBUTES> {
    /// The descriptor, kept for the process's life.
    struct Descriptor(PSECURITY_DESCRIPTOR);
    // SAFETY: the descriptor is never written once made, and lives as long as the process.
    unsafe impl Send for Descriptor {}
    // SAFETY: as above.
    unsafe impl Sync for Descriptor {}
    static MADE: OnceLock<Result<Descriptor, i32>> = OnceLock::new();
    let made = MADE.get_or_init(|| {
        let sddl = format!(
            "D:P(A;;GA;;;{})",
            user_sid().map_err(|e| e.raw_os_error().unwrap_or(-1))?
        );
        let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string, and a pointer for the call to fill, which this
        // process keeps for its life.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1));
        }
        Ok(Descriptor(descriptor))
    });
    match made {
        Ok(d) => Ok(SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: d.0,
            bInheritHandle: 0,
        }),
        Err(code) => Err(io::Error::from_raw_os_error(*code)),
    }
}

/// This process's user's SID, as SDDL writes it.
fn user_sid() -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's own token, for querying.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle just opened, which nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    // TOKEN_USER and the SID after it: room for any SID (SECURITY_MAX_SID_SIZE is 68).
    let mut buf = vec![0u64; 32];
    let mut len = 0u32;
    let size = u32::try_from(buf.len() * 8).unwrap_or(0);
    // SAFETY: a buffer of `size` bytes, aligned for TOKEN_USER, for the call to fill.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            size,
            &raw mut len,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call filled a TOKEN_USER at the buffer's start.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the SID the token names, inside `buf`; the string it makes is freed below.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a NUL-terminated string the call made.
    let units = unsafe {
        let mut n = 0;
        while *text.add(n) != 0 {
            n += 1;
        }
        std::slice::from_raw_parts(text, n)
    };
    let sid = String::from_utf16_lossy(units);
    // SAFETY: the string ConvertSidToStringSidW made, freed once.
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

/// `n` random bytes from the system's generator, as hex.
pub(crate) fn random_hex(n: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; n];
    let len = u32::try_from(n).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `len` bytes for the call to fill, with the system's preferred generator.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(io::Error::other(format!("BCryptGenRandom: {status:#x}")));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `name` as a NUL-terminated wide string.
pub(crate) fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain([0]).collect()
}

/// A new instance of the pipe `name`, duplex and overlapped, in bytes, refusing remote
/// clients, of this user's alone ([`attributes`]); its name's first instance where `first`,
/// its only where `only`.
pub(crate) fn instance(name: &[u16], first: bool, only: bool) -> io::Result<OwnedHandle> {
    let attributes = attributes()?;
    let first = if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: a NUL-terminated name, attributes that live through the call.
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            if only {
                1
            } else {
                windows_sys::Win32::System::Pipes::PIPE_UNLIMITED_INSTANCES
            },
            BUFFER,
            BUFFER,
            0,
            &raw const attributes,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the instance just made, which nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Opens the pipe `name` as its client, read and write, `overlapped` or not: its server
/// may identify this process, and no more (SECURITY_IDENTIFICATION), whatever made the name.
pub(crate) fn open(name: &[u16], overlapped: bool) -> io::Result<OwnedHandle> {
    let flags =
        SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION | if overlapped { FILE_FLAG_OVERLAPPED } else { 0 };
    // SAFETY: a NUL-terminated name and plain arguments.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle just opened, which nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// A connected pair of ends, each overlapped: a fresh name's only instance, and the client
/// end opened at once, which no other process can then open (M340).
pub(crate) fn pair() -> io::Result<(OwnedHandle, OwnedHandle)> {
    static MADE: AtomicU64 = AtomicU64::new(0);
    let name = wide(&format!(
        r"\\.\pipe\shards-{}-{}-{}",
        std::process::id(),
        MADE.fetch_add(1, Ordering::Relaxed),
        random_hex(8)?
    ));
    let server = instance(&name, true, true)?;
    let client = open(&name, true)?;
    if !connected(server.as_raw_handle(), &[])? {
        return Err(io::Error::other("a pipe pair did not connect"));
    }
    Ok((server, client))
}

/// One end of an overlapped pipe, read and written by several threads at once, each call
/// its own ([`call`]), none of them waiting out another.
#[derive(Debug)]
pub struct Pipe(OwnedHandle);

impl Pipe {
    /// Takes `handle`, an end of a pipe opened for overlapped calls.
    pub fn from_handle(handle: OwnedHandle) -> Pipe {
        Pipe(handle)
    }

    pub fn try_clone(&self) -> io::Result<Pipe> {
        Ok(Pipe(self.0.try_clone()?))
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// A read into `buf`, as `Read` reads, waiting no longer than `cancels` and `deadline`.
    pub(crate) fn read_within(
        &self,
        buf: &mut [u8],
        cancels: &[&Event],
        deadline: Option<Instant>,
    ) -> Called {
        match read_into(self.raw(), buf, cancels, deadline) {
            Called::Done(Err(e)) if ended(&e) => Called::Done(Ok(0)),
            called => called,
        }
    }

    /// All of `buf` written, or the error that cut it short; waiting no longer than `cancels`
    /// and `deadline`, which end it with `ConnectionAborted` and `TimedOut`.
    pub(crate) fn write_within(
        &self,
        mut buf: &[u8],
        cancels: &[&Event],
        deadline: Option<Instant>,
    ) -> io::Result<()> {
        while !buf.is_empty() {
            match write_from(self.raw(), buf, cancels, deadline) {
                Called::Done(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                Called::Done(Ok(n)) => buf = buf.get(n..).unwrap_or_default(),
                Called::Done(Err(e)) if ended(&e) => return Err(io::ErrorKind::BrokenPipe.into()),
                Called::Done(Err(e)) => return Err(e),
                Called::Cancelled => return Err(io::ErrorKind::ConnectionAborted.into()),
                Called::Late => return Err(io::ErrorKind::TimedOut.into()),
            }
        }
        Ok(())
    }
}

impl AsHandle for Pipe {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl Read for &Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.read_within(buf, &[], None) {
            Called::Done(done) => done,
            Called::Cancelled | Called::Late => Err(io::ErrorKind::Interrupted.into()),
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for &Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_within(buf, &[], None).map(|()| buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The handles each end of a pair in this process has yet to take: `[0]` the server end's,
/// `[1]` the client end's.
#[derive(Debug, Default)]
struct Queues([Mutex<VecDeque<OwnedHandle>>; 2]);

/// How a [`Stream`]'s messages carry handles.
#[derive(Debug)]
enum Way {
    /// Both ends in this process: a message's handles go through the pair's own queue, the
    /// one of the end at `side`'s peer, each duplicated within the process.
    Here { pair: Arc<Queues>, side: usize },
    /// Its peer is a child of this process's, which may be given handles (a process handle
    /// of PROCESS_DUP_HANDLE): each is duplicated into it, its value after the message.
    /// None comes from it.
    Gives(OwnedHandle),
    /// Given to this process by its spawner, as its peer's end: what comes carries handles
    /// as values its spawner put in this process; none goes back.
    Given,
}

/// A [`Stream`]'s end, shared by its clones.
#[derive(Debug)]
struct End {
    /// The pipe's end; none once ended both ways, closed for its peer to read its end.
    pipe: RwLock<Option<OwnedHandle>>,
    /// Set as its reads are shut: each waiting read ends, and none waits from then on.
    reads_shut: Event,
    writes_shut: Event,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
    nonblocking: AtomicBool,
    way: Mutex<Way>,
    /// One message at a time: its handles' turn in the queue is its bytes' in the pipe.
    sending: Mutex<()>,
    /// The OVERLAPPEDs of pollers' reads of no bytes on it ([`Stream::watch`]), by address:
    /// each cancelled as its reads are shut, before its handle is closed.
    watching: Mutex<Vec<usize>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One end of a pipe of shards' own, used as Unix's `UnixStream` is: see the module's
/// documentation. Its clones share it, as a socket's descriptors share the socket (an `Arc`
/// as Windows has no other way to share a socket's state between a process's handles).
#[derive(Debug)]
pub struct Stream(Arc<End>);

impl Stream {
    fn of(handle: OwnedHandle, way: Way) -> io::Result<Stream> {
        Ok(Stream(Arc::new(End {
            pipe: RwLock::new(Some(handle)),
            reads_shut: Event::new()?,
            writes_shut: Event::new()?,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            nonblocking: AtomicBool::new(false),
            way: Mutex::new(way),
            sending: Mutex::new(()),
            watching: Mutex::new(Vec::new()),
        })))
    }

    /// A connected pair, both ends this process's ([`pair`]).
    pub fn pair() -> io::Result<(Stream, Stream)> {
        let (server, client) = pair()?;
        let queues = Arc::new(Queues::default());
        Ok((
            Stream::of(
                server,
                Way::Here {
                    pair: queues.clone(),
                    side: 0,
                },
            )?,
            Stream::of(
                client,
                Way::Here {
                    pair: queues,
                    side: 1,
                },
            )?,
        ))
    }

    /// The end `handle` this process's spawner gave it, its peer's: handles come on it as
    /// values in this process, and none goes back.
    pub fn given(handle: OwnedHandle) -> io::Result<Stream> {
        Stream::of(handle, Way::Given)
    }

    /// Its peer's end is `process`'s now, a child's this process gave it to, which `process`
    /// may give handles to (a process handle of PROCESS_DUP_HANDLE): handles go to it as
    /// values, and none comes from it.
    pub fn gives_to(&self, process: OwnedHandle) {
        *lock(&self.0.way) = Way::Gives(process);
    }

    /// Another handle to its end, for a child to be given ([`crate::Given`]).
    pub fn share(&self) -> io::Result<OwnedHandle> {
        let guard = self.0.pipe.read().unwrap_or_else(PoisonError::into_inner);
        match &*guard {
            Some(handle) => handle.try_clone(),
            None => Err(io::ErrorKind::NotConnected.into()),
        }
    }

    /// A clone: the same end, as a socket's `dup` is the same socket.
    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(Stream(self.0.clone()))
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *lock(&self.0.read_timeout) = nonzero(timeout)?;
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *lock(&self.0.write_timeout) = nonzero(timeout)?;
        Ok(())
    }

    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.nonblocking.store(on, Ordering::SeqCst);
        Ok(())
    }

    /// Ends its reads (`Read`), its writes (`Write`), or both, which closes its end too once
    /// no call of it is under way: its peer reads its end after all it was sent. Each read
    /// or write waiting ends at once.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        if matches!(how, Shutdown::Read | Shutdown::Both) {
            self.0.reads_shut.set();
            // A poller's read ends too, which its poller then names: none starts from now.
            let guard = self.0.pipe.read().unwrap_or_else(PoisonError::into_inner);
            let watching = lock(&self.0.watching);
            if let Some(handle) = &*guard {
                for &at in watching.iter() {
                    // SAFETY: a poller's OVERLAPPED, which it keeps while it is listed; a
                    // read finished already is no error (ERROR_NOT_FOUND).
                    unsafe { CancelIoEx(handle.as_raw_handle(), at as *const OVERLAPPED) };
                }
            }
        }
        if matches!(how, Shutdown::Write | Shutdown::Both) {
            self.0.writes_shut.set();
        }
        if how == Shutdown::Both {
            // Each call holds the end shared while under way: each has ended by now, or
            // ends at the events just set, and none starts once they are.
            let mut end = self.0.pipe.write().unwrap_or_else(PoisonError::into_inner);
            drop(end.take());
        }
        Ok(())
    }

    /// Whether it has something to read now, or has ended: a read would not wait.
    pub fn readable(&self) -> bool {
        if self.0.reads_shut.is_set() {
            return true;
        }
        let guard = self.0.pipe.read().unwrap_or_else(PoisonError::into_inner);
        match &*guard {
            Some(handle) => !matches!(available(handle.as_raw_handle(), None), Ok(Some(0))),
            None => true,
        }
    }

    /// The first byte of what it has to read now, not taken: `None` where nothing has come,
    /// or its peer has gone.
    pub fn peek(&self) -> io::Result<Option<u8>> {
        let guard = self.0.pipe.read().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = &*guard else {
            return Ok(None);
        };
        let mut first = 0u8;
        Ok(match available(handle.as_raw_handle(), Some(&mut first))? {
            Some(n) if n > 0 => Some(first),
            _ => None,
        })
    }

    /// Waits until it has something to read, or has ended, or `cancel` is set, or `deadline`
    /// has come: whether it has. A read of no bytes waits for what comes (PM M348).
    pub fn wait_readable(&self, cancel: Option<&Event>, deadline: Option<Instant>) -> io::Result<bool> {
        let end = &*self.0;
        if end.reads_shut.is_set() {
            return Ok(true);
        }
        let guard = end.pipe.read().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = &*guard else {
            return Ok(true);
        };
        let cancels: Vec<&Event> = std::iter::once(&end.reads_shut).chain(cancel).collect();
        match read_into(handle.as_raw_handle(), &mut [], &cancels, deadline) {
            Called::Done(Ok(_)) => Ok(true),
            Called::Done(Err(e)) if ended(&e) => Ok(true),
            Called::Done(Err(e)) => Err(e),
            Called::Cancelled => Ok(end.reads_shut.is_set()),
            Called::Late => Ok(false),
        }
    }

    /// Starts a poller's read of no bytes with `ov`, whose event is set as it finishes:
    /// whether it is under way, then listed for the stream's shutdown to cancel. Where it
    /// is not, a read would not wait: something has come, or the stream has ended.
    ///
    /// # Safety
    /// `ov` is a fresh OVERLAPPED of an event, and `ov` and `into` stay where they are,
    /// neither used nor freed, until [`Stream::unwatch`] has returned.
    unsafe fn watch(&self, ov: *mut OVERLAPPED, into: *mut u8) -> bool {
        let end = &*self.0;
        let guard = end.pipe.read().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = &*guard else {
            return false;
        };
        let mut watching = lock(&end.watching);
        // Under the lock a shutdown cancels under: none starts once its reads are shut.
        if end.reads_shut.is_set() {
            return false;
        }
        // SAFETY: a read of no bytes, its OVERLAPPED and buffer the caller's to keep.
        let started = unsafe { ReadFile(handle.as_raw_handle(), into, 0, std::ptr::null_mut(), ov) };
        // SAFETY: no arguments.
        if started != 0 || unsafe { GetLastError() } != ERROR_IO_PENDING {
            return false;
        }
        let at = ov as usize;
        if !watching.contains(&at) {
            watching.push(at);
        }
        true
    }

    /// Ends the read [`Stream::watch`] started with `ov`, where it is under way, and waits
    /// for it to finish: `ov` is its caller's again.
    ///
    /// # Safety
    /// `ov` is the one of a read `watch` started, or of none, and `event` its event.
    unsafe fn unwatch(&self, ov: *mut OVERLAPPED, event: &Event) {
        let end = &*self.0;
        {
            let guard = end.pipe.read().unwrap_or_else(PoisonError::into_inner);
            lock(&end.watching).retain(|&at| at != ov as usize);
            if let Some(handle) = &*guard {
                // SAFETY: the caller's OVERLAPPED; a read finished already is no error.
                unsafe { CancelIoEx(handle.as_raw_handle(), ov) };
            }
        }
        // Cancelled here, or as the stream's reads were shut: it finishes.
        // SAFETY: the caller's OVERLAPPED, whose status the system writes as the read ends.
        while unsafe { std::ptr::read_volatile(&raw const (*ov).Internal) } == STATUS_PENDING as usize {
            // SAFETY: an event this process holds, set as the read finishes.
            unsafe { WaitForSingleObject(event.raw(), INFINITE) };
        }
    }

    /// A key its clones share, while one lives.
    fn key(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// A read into `buf` by `deadline`, if one is given (`TimedOut` past it), else within
    /// its read timeout (`WouldBlock` past that).
    fn read_by(&self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
        let end = &*self.0;
        if end.reads_shut.is_set() {
            return Ok(0);
        }
        let guard = end.pipe.read().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = &*guard else {
            return Ok(0);
        };
        let handle = handle.as_raw_handle();
        if end.nonblocking.load(Ordering::SeqCst) {
            match available(handle, None)? {
                None => return Ok(0),
                Some(0) => return Err(io::ErrorKind::WouldBlock.into()),
                Some(_) => {}
            }
        }
        let timeout = lock(&end.read_timeout).and_then(|t| Instant::now().checked_add(t));
        let (until, late) = match (deadline, timeout) {
            (Some(d), Some(t)) if t < d => (Some(t), io::ErrorKind::WouldBlock),
            (Some(d), _) => (Some(d), io::ErrorKind::TimedOut),
            (None, t) => (t, io::ErrorKind::WouldBlock),
        };
        match read_into(handle, buf, &[&end.reads_shut], until) {
            Called::Done(Ok(n)) => Ok(n),
            Called::Done(Err(e)) if ended(&e) => Ok(0),
            Called::Done(Err(e)) => Err(e),
            Called::Cancelled => Ok(0),
            Called::Late => Err(late.into()),
        }
    }

    /// Writes all of `buf`, within its write timeout (`WouldBlock` past it); a shut writing
    /// end, or one whose peer has gone, is a broken pipe.
    fn write_all_now(&self, buf: &[u8]) -> io::Result<()> {
        let end = &*self.0;
        if end.writes_shut.is_set() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let guard = end.pipe.read().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = &*guard else {
            return Err(io::ErrorKind::BrokenPipe.into());
        };
        let deadline = lock(&end.write_timeout).and_then(|t| Instant::now().checked_add(t));
        let mut left = buf;
        while !left.is_empty() {
            match write_from(handle.as_raw_handle(), left, &[&end.writes_shut], deadline) {
                Called::Done(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                Called::Done(Ok(n)) => left = left.get(n..).unwrap_or_default(),
                Called::Done(Err(e)) if ended(&e) => return Err(io::ErrorKind::BrokenPipe.into()),
                Called::Done(Err(e)) => return Err(e),
                Called::Cancelled => return Err(io::ErrorKind::BrokenPipe.into()),
                Called::Late => return Err(io::ErrorKind::WouldBlock.into()),
            }
        }
        Ok(())
    }
}

/// Its end's handle's value, as a Unix socket's descriptor is given: one that names its
/// end only while it is open, as its holder's to make sure of. None is borrowed
/// (`AsHandle`): ending it both ways closes the end whatever borrows it.
impl std::os::windows::io::AsRawHandle for Stream {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        let guard = self.0.pipe.read().unwrap_or_else(PoisonError::into_inner);
        guard
            .as_ref()
            .map_or(INVALID_HANDLE_VALUE, |handle| handle.as_raw_handle())
    }
}

/// Whether the pipe end `handle` has something to read now, or has ended: a read would not
/// wait. One that is no pipe's end now reads as ended.
pub fn readable_raw(handle: std::os::windows::io::RawHandle) -> bool {
    !matches!(available(handle, None), Ok(Some(0)))
}

/// Waits until one of `streams` has something to read, or has ended, or `deadline` has
/// come: which have, each looked at as the wait ends, as poll(2) answers. Each is waited on
/// by a read of no bytes (M348), every one cancelled and finished before this returns.
pub fn await_streams(streams: &[&Stream], deadline: Option<Instant>) -> io::Result<Vec<bool>> {
    Ok(await_any(streams, &[], deadline)?.0)
}

/// Waits as [`await_streams`] does, and for one of `events` (waitable objects: an event, a
/// process) to be signalled: which streams are ready, and which events are signalled.
pub fn await_any(
    streams: &[&Stream],
    events: &[BorrowedHandle<'_>],
    deadline: Option<Instant>,
) -> io::Result<(Vec<bool>, Vec<bool>)> {
    // Its wait takes MAXIMUM_WAIT_OBJECTS (64) at most: a read's event and the stream's own
    // for each stream, and each of `events`.
    if streams.len().saturating_mul(2).saturating_add(events.len()) > 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "more streams and events than one wait takes",
        ));
    }
    let signalled = |event: &BorrowedHandle<'_>| {
        // SAFETY: a waitable object the caller holds, not waited for.
        unsafe { WaitForSingleObject(event.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    };
    let ready = |streams: &[&Stream]| {
        (
            streams.iter().map(|s| s.readable()).collect::<Vec<bool>>(),
            events.iter().map(signalled).collect::<Vec<bool>>(),
        )
    };
    let now = ready(streams);
    if now.0.iter().chain(&now.1).any(|&r| r) {
        return Ok(now);
    }
    let guards: Vec<_> = streams
        .iter()
        .map(|s| s.0.pipe.read().unwrap_or_else(PoisonError::into_inner))
        .collect();
    let mut reads = Vec::with_capacity(streams.len());
    for _ in streams {
        reads.push(Event::new()?);
    }
    // Each read's OVERLAPPED, where it stays until the read has finished.
    let mut overlapped: Vec<OVERLAPPED> = reads
        .iter()
        .map(|e| OVERLAPPED {
            hEvent: e.raw(),
            ..OVERLAPPED::default()
        })
        .collect();
    // What the reads of no bytes are given to read into, kept until each has finished.
    let mut none = [0u8; 1];
    let mut started = Vec::with_capacity(streams.len());
    for (guard, ov) in guards.iter().zip(overlapped.iter_mut()) {
        let Some(handle) = guard.as_ref() else {
            started.push(None);
            continue;
        };
        let raw = handle.as_raw_handle();
        // SAFETY: a read of no bytes into `none`, its OVERLAPPED kept in `overlapped`, each
        // neither moved nor dropped until the read has finished, below.
        let ok = unsafe { ReadFile(raw, none.as_mut_ptr(), 0, std::ptr::null_mut(), ov) };
        // SAFETY: no arguments.
        let pending = ok != 0 || unsafe { GetLastError() } == ERROR_IO_PENDING;
        started.push(pending.then_some(raw));
    }
    let waited: Vec<HANDLE> = reads
        .iter()
        .map(Event::raw)
        .chain(streams.iter().map(|s| s.0.reads_shut.raw()))
        .chain(events.iter().map(|e| e.as_raw_handle()))
        .collect();
    let ms = deadline.map_or(INFINITE, |d| {
        let left = d.saturating_duration_since(Instant::now());
        u32::try_from(left.as_micros().div_ceil(1000))
            .unwrap_or(INFINITE - 1)
            .min(INFINITE - 1)
    });
    // Every read not started has ended or failed at once, which a look sees.
    if started.iter().all(Option::is_some) {
        let n = u32::try_from(waited.len()).unwrap_or(1);
        // SAFETY: events this function and the streams hold, `n` of them.
        unsafe { WaitForMultipleObjects(n, waited.as_ptr(), 0, ms) };
    }
    for (raw, ov) in started.iter().zip(overlapped.iter()) {
        if let Some(raw) = raw {
            let _ = cancelled(*raw, ov, Called::Cancelled);
        }
    }
    drop(guards);
    Ok(ready(streams))
}

/// A timeout as a socket takes one: none for `None`, refused for zero (std's
/// `set_read_timeout`).
fn nonzero(timeout: Option<Duration>) -> io::Result<Option<Duration>> {
    match timeout {
        Some(t) if t.is_zero() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot set a 0 duration timeout",
        )),
        t => Ok(t),
    }
}

impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_by(buf, None)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_by(buf, None)
    }
}

impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all_now(buf).map(|()| buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A message's header on a stream: its kind, its payload's length (a big-endian u32), and
/// how many handles it carries.
const HEADER: usize = 6;

/// A received message: its kind, its payload, and the handles that came with it, this
/// process's own.
#[derive(Debug)]
pub struct Message {
    pub kind: u8,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedHandle>,
}

/// Sends one message on `sock` with `fds`, which go to its peer's process ([`Way`]); this
/// process keeps its own. Blocks until all of it is written.
pub fn send(sock: &Stream, kind: u8, payload: &[u8], fds: &[BorrowedHandle<'_>]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message payload too large",
        ));
    }
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors for one message",
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("payload length"))?;
    let count = u8::try_from(fds.len()).map_err(|_| io::Error::other("descriptor count"))?;
    let _turn = lock(&sock.0.sending);
    let mut bytes = Vec::with_capacity(HEADER + payload.len() + fds.len() * 8);
    bytes.push(kind);
    bytes.extend(len.to_be_bytes());
    bytes.push(count);
    bytes.extend_from_slice(payload);
    let way = lock(&sock.0.way);
    match &*way {
        Way::Here { pair, side } => {
            if !fds.is_empty() {
                let theirs = pair.0.get(1 - side).ok_or(io::ErrorKind::InvalidInput)?;
                let copies = fds
                    .iter()
                    .map(|fd| fd.try_clone_to_owned())
                    .collect::<io::Result<Vec<_>>>()?;
                lock(theirs).extend(copies);
            }
            drop(way);
            sock.write_all_now(&bytes)
        }
        Way::Gives(process) => {
            // A handle of its own to the child: the way's lock is not held across a write,
            // which waits as long as the peer leaves it unread, nor a read here meanwhile.
            let process = process.try_clone()?;
            drop(way);
            let process = &process;
            let mut given = Vec::with_capacity(fds.len());
            for fd in fds {
                match give(process, *fd) {
                    Ok(value) => given.push(value),
                    Err(e) => {
                        take_back(process, &given);
                        return Err(e);
                    }
                }
            }
            for value in &given {
                bytes.extend(value.to_le_bytes());
            }
            let sent = sock.write_all_now(&bytes);
            // Not all of it read, it was not taken: what was given is closed there.
            if sent.is_err() {
                take_back(process, &given);
            }
            sent
        }
        Way::Given if fds.is_empty() => {
            drop(way);
            sock.write_all_now(&bytes)
        }
        Way::Given => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no handle goes to this stream's peer",
        )),
    }
}

/// `handle` duplicated into `process`: its value there.
fn give(process: &OwnedHandle, handle: BorrowedHandle<'_>) -> io::Result<u64> {
    let mut theirs: HANDLE = std::ptr::null_mut();
    // SAFETY: a handle of ours, duplicated into a process we may give handles to.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.as_raw_handle(),
            process.as_raw_handle(),
            &raw mut theirs,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(theirs as u64)
}

/// Closes the handles `values` this process gave `process`, a message of which went unread.
fn take_back(process: &OwnedHandle, values: &[u64]) {
    for &value in values {
        // SAFETY: a value this process put in `process`, closed there and duplicated nowhere.
        unsafe {
            DuplicateHandle(
                process.as_raw_handle(),
                value as HANDLE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                0,
                DUPLICATE_CLOSE_SOURCE,
            )
        };
    }
}

/// Receives one message: `None` where its peer ended between messages.
pub fn recv(sock: &Stream) -> io::Result<Option<Message>> {
    recv_until(sock, None)
}

/// [`recv`], if all of the message arrives by `deadline`; `TimedOut` then otherwise.
pub fn recv_by(sock: &Stream, deadline: Instant) -> io::Result<Option<Message>> {
    recv_until(sock, Some(deadline))
}

fn recv_until(sock: &Stream, deadline: Option<Instant>) -> io::Result<Option<Message>> {
    let mut header = [0u8; HEADER];
    let got = loop {
        match sock.read_by(&mut header, deadline) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            got => break got?,
        }
    };
    if got == 0 {
        return Ok(None);
    }
    fill(sock, header.get_mut(got..).unwrap_or_default(), deadline)?;
    let [kind, l0, l1, l2, l3, count] = header;
    let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message payload too large",
        ));
    }
    let count = usize::from(count);
    if count > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "more descriptors than a message may carry",
        ));
    }
    let mut payload = vec![0u8; len];
    fill(sock, &mut payload, deadline)?;
    let way = lock(&sock.0.way);
    let fds = match &*way {
        Way::Here { pair, side } => {
            let ours = pair.0.get(*side).ok_or(io::ErrorKind::InvalidInput)?;
            let mut queued = lock(ours);
            if queued.len() < count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a message's descriptors never came",
                ));
            }
            queued.drain(..count).collect()
        }
        Way::Given => {
            let mut values = vec![0u8; count * 8];
            drop(way);
            fill(sock, &mut values, deadline)?;
            values
                .as_chunks::<8>()
                .0
                .iter()
                .map(|v| {
                    let raw = u64::from_le_bytes(*v);
                    // SAFETY: a handle this process's spawner put here for this message,
                    // owned from here on, as Unix owns a descriptor SCM_RIGHTS brings.
                    unsafe { OwnedHandle::from_raw_handle(raw as HANDLE) }
                })
                .collect()
        }
        Way::Gives(_) if count == 0 => Vec::new(),
        Way::Gives(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a message named handles its sender may not give",
            ));
        }
    };
    Ok(Some(Message { kind, payload, fds }))
}

/// Reads exactly `buf.len()` more bytes of a message.
fn fill(sock: &Stream, mut buf: &mut [u8], deadline: Option<Instant>) -> io::Result<()> {
    while !buf.is_empty() {
        let n = match sock.read_by(buf, deadline) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            n => n?,
        };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = buf.get_mut(n..).unwrap_or_default();
    }
    Ok(())
}

/// Ends `sock` both ways: its peer reads its end, and each read and write of it here ends.
pub fn end(sock: &Stream) {
    let _ = sock.shutdown(Shutdown::Both);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    use windows_sys::Win32::Foundation::{ERROR_BAD_PIPE, ERROR_FILE_NOT_FOUND, ERROR_SEM_TIMEOUT};
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_DUP_HANDLE};

    /// The Win32 error `e` carries.
    fn code(e: &io::Error) -> u32 {
        u32::try_from(e.raw_os_error().unwrap()).unwrap()
    }

    /// A name no other test's pipe has.
    fn fresh(what: &str) -> Vec<u16> {
        wide(&format!(
            r"\\.\pipe\shards-test-{what}-{}-{}",
            std::process::id(),
            random_hex(8).unwrap()
        ))
    }

    /// A message and a handle go from one end to the other, the handle the receiver's own;
    /// those between ends of one process go through the pair's queue.
    #[test]
    fn a_pairs_ends_carry_messages_and_their_handles() {
        let (a, b) = Stream::pair().unwrap();
        let event = Event::new().unwrap();
        send(&a, 7, b"ps", &[event.as_handle()]).unwrap();
        send(&a, 8, b"", &[]).unwrap();
        let m = recv(&b).unwrap().unwrap();
        assert_eq!((m.kind, m.payload.as_slice(), m.fds.len()), (7, &b"ps"[..], 1));
        // The receiver's own handle to the sender's event.
        let theirs = Event(m.fds.into_iter().next().unwrap());
        assert_ne!(theirs.raw(), event.raw());
        theirs.set();
        assert!(event.is_set());
        let m = recv(&b).unwrap().unwrap();
        assert_eq!((m.kind, m.fds.len()), (8, 0));
        // Back the other way.
        send(&b, 9, b"back", &[]).unwrap();
        assert_eq!(recv(&a).unwrap().unwrap().payload, b"back");
    }

    /// An end ended is read to its last message, then its end; a dropped one too.
    #[test]
    fn an_ends_end_is_read_after_all_it_sent() {
        for dropped in [false, true] {
            let (a, b) = Stream::pair().unwrap();
            for kind in 1..=3 {
                send(&a, kind, &[kind; 100], &[]).unwrap();
            }
            if dropped {
                drop(a);
            } else {
                end(&a);
                assert!(send(&a, 4, b"", &[]).is_err(), "sent once ended");
            }
            for kind in 1..=3 {
                assert_eq!(recv(&b).unwrap().unwrap().kind, kind);
            }
            assert!(recv(&b).unwrap().is_none());
        }
    }

    /// Its reads shut, a read waiting on a clone ends at once with its end; and so does
    /// one waiting as its stream is ended both ways.
    #[test]
    fn a_shut_ends_reads_waiting_on_its_clones() {
        for how in [Shutdown::Read, Shutdown::Both] {
            let (a, _b) = Stream::pair().unwrap();
            let reading = a.try_clone().unwrap();
            std::thread::scope(|s| {
                let read = s.spawn(move || recv(&reading));
                std::thread::sleep(Duration::from_millis(50));
                assert!(!read.is_finished(), "a read did not wait");
                let t0 = Instant::now();
                a.shutdown(how).unwrap();
                assert!(read.join().unwrap().unwrap().is_none());
                assert!(t0.elapsed() < Duration::from_secs(1));
            });
        }
    }

    /// A read waits as long as its timeout (`WouldBlock`), a message as long as its deadline
    /// (`TimedOut`); then a message that comes is taken whole.
    #[test]
    fn reads_wait_no_longer_than_asked() {
        let (a, b) = Stream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let t0 = Instant::now();
        let e = recv(&b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert!(t0.elapsed() >= Duration::from_millis(50));
        b.set_read_timeout(None).unwrap();
        assert!(b.set_read_timeout(Some(Duration::ZERO)).is_err());
        let e = recv_by(&b, Instant::now() + Duration::from_millis(50)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        send(&a, 3, b"late", &[]).unwrap();
        let m = recv_by(&b, Instant::now() + Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(m.kind, 3);
        b.set_nonblocking(true).unwrap();
        assert_eq!(
            (&b).read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    /// A read of no bytes waits for what comes, as a readiness wait (PM M348): not ready
    /// with nothing there, ready once a byte is, nothing of it taken; and ready once its
    /// peer has gone.
    #[test]
    fn a_stream_waits_to_be_readable() {
        let (a, b) = Stream::pair().unwrap();
        let soon = || Some(Instant::now() + Duration::from_millis(50));
        assert!(!b.wait_readable(None, soon()).unwrap());
        assert!(!b.readable());
        (&a).write_all(b"x").unwrap();
        assert!(b.wait_readable(None, soon()).unwrap());
        assert!(b.readable());
        assert_eq!(b.peek().unwrap(), Some(b'x'));
        let mut got = [0u8; 1];
        (&b).read_exact(&mut got).unwrap();
        assert!(!b.wait_readable(None, soon()).unwrap());
        // Woken by a cancel of the caller's.
        let cancel = Event::new().unwrap();
        cancel.set();
        assert!(!b.wait_readable(Some(&cancel), None).unwrap());
        drop(a);
        assert!(b.wait_readable(None, soon()).unwrap());
        assert!(b.readable());
    }

    /// One thread's write goes while another's read of the same end waits: each call is
    /// its own (D140, M207).
    #[test]
    fn a_waiting_read_holds_up_no_write() {
        let (a, b) = Stream::pair().unwrap();
        let reader = a.try_clone().unwrap();
        std::thread::scope(|s| {
            let read = s.spawn(move || recv(&reader).unwrap().unwrap().kind);
            std::thread::sleep(Duration::from_millis(50));
            send(&a, 1, b"while it reads", &[]).unwrap();
            assert_eq!(recv(&b).unwrap().unwrap().kind, 1);
            send(&b, 2, b"", &[]).unwrap();
            assert_eq!(read.join().unwrap(), 2);
        });
    }

    /// A message past the pipe's buffer goes whole to a reader that takes it as it comes.
    #[test]
    fn a_message_past_the_buffer_goes_whole() {
        let (a, b) = Stream::pair().unwrap();
        let big: Vec<u8> = (0..MAX_PAYLOAD).map(|i| (i % 251) as u8).collect();
        std::thread::scope(|s| {
            let sent = s.spawn(|| send(&a, 5, &big, &[]));
            let m = recv(&b).unwrap().unwrap();
            assert_eq!(m.payload, big);
            sent.join().unwrap().unwrap();
        });
    }

    /// Handles come only where they may: a child's end takes the values its spawner put in
    /// its process; its spawner's end refuses any its child names, and an end of this
    /// process's any its queue does not hold.
    #[test]
    fn handles_come_only_from_where_they_may() {
        // This process plays the child: its own handle, to be given handles by.
        // SAFETY: OpenProcess of this process, owned below.
        let me = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, std::process::id()) };
        assert!(!me.is_null());
        // SAFETY: the handle just opened.
        let me = unsafe { OwnedHandle::from_raw_handle(me) };
        let (spawner, child) = Stream::pair().unwrap();
        let child = Stream::given(child.share().unwrap()).unwrap();
        spawner.gives_to(me);
        let event = Event::new().unwrap();
        send(&spawner, 1, b"run", &[event.as_handle()]).unwrap();
        let m = recv(&child).unwrap().unwrap();
        let given = Event(m.fds.into_iter().next().unwrap());
        given.set();
        assert!(event.is_set());
        // None goes back, and none named back is taken.
        assert!(send(&child, 2, b"", &[event.as_handle()]).is_err());
        (&child)
            .write_all(&[2, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(recv(&spawner).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Between ends of one process, a count the queue does not hold is refused.
        let (a, b) = Stream::pair().unwrap();
        (&a).write_all(&[3, 0, 0, 0, 0, 1]).unwrap();
        assert_eq!(recv(&b).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// What slots rely on (PM M348): an instance no client has opened peeks as a bad pipe
    /// (ERROR_BAD_PIPE, not ERROR_PIPE_LISTENING); one opened, as connected with nothing to
    /// read; one its client closed, as ended.
    #[test]
    fn an_instance_peeks_as_listening_until_opened() {
        let name = fresh("peek");
        let server = instance(&name, true, false).unwrap();
        let e = available(server.as_raw_handle(), None).unwrap_err();
        assert_eq!(code(&e), ERROR_BAD_PIPE);
        let client = open(&name, true).unwrap();
        assert_eq!(available(server.as_raw_handle(), None).unwrap(), Some(0));
        drop(client);
        assert_eq!(available(server.as_raw_handle(), None).unwrap(), None);
    }

    /// What a client waiting for a free instance relies on (PM M348): WaitNamedPipe waits
    /// while every instance is taken, and ends as soon as one is free; with no instance at
    /// all, at once.
    #[test]
    fn a_client_waits_for_a_free_instance() {
        let name = fresh("wait");
        // SAFETY: a NUL-terminated name.
        let waited = |ms| unsafe { WaitNamedPipeW(name.as_ptr(), ms) };
        assert_eq!(waited(1000), 0);
        assert_eq!(code(&io::Error::last_os_error()), ERROR_FILE_NOT_FOUND);
        let first = instance(&name, true, false).unwrap();
        let _taken = open(&name, true).unwrap();
        let t0 = Instant::now();
        assert_eq!(waited(50), 0);
        assert_eq!(code(&io::Error::last_os_error()), ERROR_SEM_TIMEOUT);
        assert!(t0.elapsed() >= Duration::from_millis(40));
        std::thread::scope(|s| {
            let waiting = s.spawn(|| {
                let t0 = Instant::now();
                (waited(10_000), t0.elapsed())
            });
            std::thread::sleep(Duration::from_millis(100));
            let _second = instance(&name, false, false).unwrap();
            let (got, took) = waiting.join().unwrap();
            assert_ne!(got, 0);
            assert!(
                took < Duration::from_secs(5),
                "waited {took:?} for a free instance"
            );
        });
        drop(first);
    }

    /// What a client waiting on a daemon that goes relies on (PM M348): a WaitNamedPipe
    /// under way ends as the last instance closes, the name not found.
    #[test]
    fn a_wait_ends_as_the_last_instance_closes() {
        let name = fresh("gone");
        let server = instance(&name, true, false).unwrap();
        let _taken = open(&name, true).unwrap();
        std::thread::scope(|s| {
            let waiting = s.spawn(|| {
                let t0 = Instant::now();
                // SAFETY: a NUL-terminated name.
                let got = unsafe { WaitNamedPipeW(name.as_ptr(), 3000) };
                (got, io::Error::last_os_error(), t0.elapsed())
            });
            std::thread::sleep(Duration::from_millis(100));
            drop(server);
            let (got, e, took) = waiting.join().unwrap();
            assert_eq!(got, 0);
            assert_eq!(code(&e), ERROR_FILE_NOT_FOUND);
            assert!(took < Duration::from_secs(1), "a wait ended {took:?} in");
        });
    }

    /// An event set wakes a thread waiting on another handle to it; reset, it is not set.
    #[test]
    fn an_event_wakes_its_waiters() {
        let event = Event::new().unwrap();
        let theirs = event.try_clone().unwrap();
        std::thread::scope(|s| {
            let waiting = s.spawn(|| {
                // SAFETY: an event this test holds.
                unsafe { WaitForSingleObject(theirs.raw(), 5000) }
            });
            std::thread::sleep(Duration::from_millis(20));
            event.set();
            assert_eq!(waiting.join().unwrap(), WAIT_OBJECT_0);
        });
        event.reset();
        assert!(!event.is_set());
    }
}
