//! Streams waited on together for something to read, or their end, and processes for their
//! end, each named by a token: Windows' twin of `shards_vmm::platform::Poller`, on which
//! shards' daemon follows every run on one thread (PM M89, M90). A stream is watched by a
//! read of no bytes of its own, which finishes as something comes or the stream ends
//! (M348), and a process by its handle; a thread-pool wait on each (CreateThreadpoolWait)
//! posts it to a completion port as it is signalled, and a wait takes the port's packets:
//! it costs what is ready, not what is watched. Level by level: a stream named by a wait
//! is looked at again by the next, its read started again, and named again at once where
//! something is left to read or it has ended.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
    PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::{
    CloseThreadpoolWait, CreateThreadpoolWait, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE,
    PTP_CALLBACK_INSTANCE, PTP_WAIT, SetThreadpoolWait, WaitForThreadpoolWaitCallbacks,
};

use super::{Event, Stream, lock};

/// Packets a wait takes at most; more wait for the next.
const AT_ONCE: usize = 64;

/// What a packet tells, as its count of bytes: a stream's read has finished, a stream was
/// ready as it was added, or a process has ended.
const FINISHED: u32 = 1;
const READY: u32 = 2;
const ENDED: u32 = 3;

/// What a thread-pool wait posts, kept where its callback finds it until the wait is gone:
/// the port, the token, what it tells, and the watch it is of (`id`, never used again),
/// which a packet carries as its OVERLAPPED's value.
#[derive(Debug)]
struct Post {
    port: HANDLE,
    key: usize,
    tells: u32,
    id: usize,
}

/// A thread-pool wait that posts its [`Post`] each time it is set and what it waits on is
/// signalled.
#[derive(Debug)]
struct Wait {
    wait: PTP_WAIT,
    post: Box<Post>,
}

// SAFETY: a thread-pool wait is set, waited out and closed from any thread; the port's
// handle in its Post is only posted to.
unsafe impl Send for Wait {}
// SAFETY: as above: shared, it is only read.
unsafe impl Sync for Wait {}

/// Posts a wait's packet to its port.
unsafe extern "system" fn posted(
    _: PTP_CALLBACK_INSTANCE,
    context: *mut core::ffi::c_void,
    _: PTP_WAIT,
    _: u32,
) {
    // SAFETY: the Post the wait was made with, which outlives the wait.
    let post = unsafe { &*context.cast::<Post>() };
    // SAFETY: a port held open while the wait lives; the OVERLAPPED's value is the watch's
    // ID, never read through.
    unsafe {
        PostQueuedCompletionStatus(
            post.port,
            post.tells,
            post.key,
            std::ptr::without_provenance(post.id),
        )
    };
}

impl Wait {
    fn new(post: Post) -> io::Result<Wait> {
        let post = Box::new(post);
        let context: *const Post = &*post;
        // SAFETY: a wait whose callback reads `post`, which lives as long as it does.
        let wait = unsafe { CreateThreadpoolWait(Some(posted), context.cast_mut().cast(), std::ptr::null()) };
        if wait == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Wait { wait, post })
    }

    /// Waits once for `handle` to be signalled, which posts the packet.
    fn set(&self, handle: HANDLE) {
        // SAFETY: our wait, on a handle its caller keeps open while it is set; no timeout.
        unsafe { SetThreadpoolWait(self.wait, handle, std::ptr::null()) };
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        // SAFETY: our wait: unset, its callbacks waited out, those not begun cancelled, and
        // closed, before its Post goes.
        unsafe {
            SetThreadpoolWait(self.wait, std::ptr::null_mut(), std::ptr::null());
            WaitForThreadpoolWaitCallbacks(self.wait, 1);
            CloseThreadpoolWait(self.wait);
        }
    }
}

/// A read of no bytes, kept where it is while under way.
struct Nothing {
    ov: UnsafeCell<OVERLAPPED>,
    into: UnsafeCell<u8>,
    event: Event,
}

/// A stream watched: the wait that posts its read's finish, the read, and whether the read
/// is under way, the wait set for it. In the order they go: the wait before the event it
/// waits on, the read once it has finished.
struct Watched {
    wait: Wait,
    read: Box<Nothing>,
    stream: Stream,
    armed: bool,
}

// SAFETY: its read's OVERLAPPED and byte are touched only by the system while it is under
// way and, between reads, by whoever holds the poller's lock.
unsafe impl Send for Watched {}

impl Watched {
    /// Starts its read again: whether a read would not wait, something there or its end.
    fn arm(&mut self) -> bool {
        let read = &*self.read;
        // SAFETY: no read of it is under way: its OVERLAPPED is ours to make fresh.
        unsafe {
            read.ov.get().write(OVERLAPPED {
                hEvent: read.event.raw(),
                ..OVERLAPPED::default()
            })
        };
        // SAFETY: `read` is boxed, kept until `unwatch` in our drop has returned.
        if unsafe { self.stream.watch(read.ov.get(), read.into.get()) } {
            self.wait.set(read.event.raw());
            self.armed = true;
            false
        } else {
            true
        }
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        // Its read ended before its fields go. Its wait, which the read's end may set off,
        // posts for a watch no longer there.
        // SAFETY: the OVERLAPPED of its read, with its event.
        unsafe { self.stream.unwatch(self.read.ov.get(), &self.read.event) };
    }
}

/// What a poller watches.
#[derive(Default)]
struct Watches {
    streams: HashMap<u64, Watched>,
    /// Each stream's token, by its end ([`Stream::key`]).
    tokens: HashMap<usize, u64>,
    /// Streams whose reads have finished, each looked at again by the next wait.
    finished: Vec<u64>,
    /// The ID of the next watch.
    next: usize,
}

/// Streams and processes waited on together: see the module's documentation.
pub struct Poller {
    /// Before the port: every wait posting to it goes first.
    watches: std::sync::Mutex<Watches>,
    port: OwnedHandle,
}

impl std::fmt::Debug for Poller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Poller")
            .field("streams", &lock(&self.watches).streams.len())
            .finish()
    }
}

/// A process's end, watched ([`Poller::add_exit`]) until this is dropped.
#[derive(Debug)]
pub struct ExitWatch {
    /// Before the handles: it waits on the process, and posts to the port.
    _wait: Wait,
    _process: OwnedHandle,
    _port: OwnedHandle,
}

impl Poller {
    pub fn new() -> io::Result<Poller> {
        // SAFETY: a new port, owned below; one thread takes its packets at a time.
        let raw = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Poller {
            watches: std::sync::Mutex::new(Watches {
                next: 1,
                ..Watches::default()
            }),
            // SAFETY: a handle just made, which nothing else owns.
            port: unsafe { OwnedHandle::from_raw_handle(raw) },
        })
    }

    fn key(token: u64) -> io::Result<usize> {
        usize::try_from(token).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a token past a key"))
    }

    /// Watches `stream`, named `token`, until it is [`remove`](Self::remove)d: ready while
    /// it has something to read, or once it has ended.
    pub fn add(&self, stream: &Stream, token: u64) -> io::Result<()> {
        let key = Poller::key(token)?;
        let mut w = lock(&self.watches);
        if w.tokens.contains_key(&stream.key()) || w.streams.contains_key(&token) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let id = w.next;
        w.next = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("watches past counting"))?;
        let mut watched = Watched {
            wait: Wait::new(Post {
                port: self.port.as_raw_handle(),
                key,
                tells: FINISHED,
                id,
            })?,
            read: Box::new(Nothing {
                ov: UnsafeCell::new(OVERLAPPED::default()),
                into: UnsafeCell::new(0),
                event: Event::new()?,
            }),
            stream: stream.try_clone()?,
            armed: false,
        };
        if watched.arm() {
            // Ready now: a wait under way is told, and the next looks at it again.
            // SAFETY: our port; the OVERLAPPED's value is the watch's ID, never read through.
            if unsafe {
                PostQueuedCompletionStatus(
                    self.port.as_raw_handle(),
                    READY,
                    key,
                    std::ptr::without_provenance(id),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            w.finished.push(token);
        }
        w.tokens.insert(stream.key(), token);
        w.streams.insert(token, watched);
        Ok(())
    }

    /// Stops watching `stream`.
    pub fn remove(&self, stream: &Stream) -> io::Result<()> {
        let watched = {
            let mut w = lock(&self.watches);
            let token = w.tokens.remove(&stream.key()).ok_or(io::ErrorKind::NotFound)?;
            w.finished.retain(|&t| t != token);
            w.streams.remove(&token)
        };
        // Its read ended, and its wait, outside the lock: a wait under way goes on.
        drop(watched);
        Ok(())
    }

    /// Watches process `pid` for its end, named `token`, until the watch is dropped: ready
    /// once, as it ends, or at once where it has. The caller holds a handle of the process
    /// (its `Child`), so that its ID names no other.
    pub fn add_exit(&self, pid: u32, token: u64) -> io::Result<ExitWatch> {
        let key = Poller::key(token)?;
        // SAFETY: OpenProcess for its SYNCHRONIZE right alone, not inherited.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a handle just opened, which nothing else owns.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        // Its own handle of the port: its wait may post after the poller has gone.
        let port = self.port.try_clone()?;
        let wait = Wait::new(Post {
            port: port.as_raw_handle(),
            key,
            tells: ENDED,
            id: 0,
        })?;
        wait.set(process.as_raw_handle());
        Ok(ExitWatch {
            _wait: wait,
            _process: process,
            _port: port,
        })
    }

    /// Waits until some of those watched are ready, or `timeout` passes (`None`: for as
    /// long as it takes), and puts their tokens in `ready`, which it empties first.
    pub fn wait(&self, ready: &mut Vec<u64>, timeout: Option<Duration>) -> io::Result<()> {
        ready.clear();
        // None: for as long as it takes, past an Instant's reach too.
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        {
            let mut w = lock(&self.watches);
            let Watches {
                streams, finished, ..
            } = &mut *w;
            for token in std::mem::take(finished) {
                if let Some(watched) = streams.get_mut(&token)
                    && !watched.armed
                    && watched.arm()
                {
                    ready.push(token);
                    finished.push(token);
                }
            }
        }
        let mut entries = [OVERLAPPED_ENTRY::default(); AT_ONCE];
        loop {
            let ms = match deadline {
                _ if !ready.is_empty() => 0,
                None => INFINITE,
                // Rounded up: a wait of 0 ms would end at once, and spin.
                Some(d) => u32::try_from(
                    d.saturating_duration_since(Instant::now())
                        .as_micros()
                        .div_ceil(1000),
                )
                .unwrap_or(INFINITE - 1)
                .min(INFINITE - 1),
            };
            let mut got = 0u32;
            // SAFETY: our port, and room for AT_ONCE entries; not alertable.
            let took = unsafe {
                GetQueuedCompletionStatusEx(
                    self.port.as_raw_handle(),
                    entries.as_mut_ptr(),
                    AT_ONCE as u32,
                    &raw mut got,
                    ms,
                    0,
                )
            };
            if took == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != i32::try_from(WAIT_TIMEOUT).ok() {
                    return Err(e);
                }
            } else {
                let mut w = lock(&self.watches);
                let Watches {
                    streams, finished, ..
                } = &mut *w;
                for entry in entries.iter().take(usize::try_from(got).unwrap_or(0)) {
                    let token = entry.lpCompletionKey as u64;
                    let tells = entry.dwNumberOfBytesTransferred;
                    // A stream's, of a watch removed or made anew since, is let go of.
                    let named = tells == ENDED
                        || streams.get_mut(&token).is_some_and(|watched| {
                            if watched.wait.post.id != entry.lpOverlapped.addr() {
                                return false;
                            }
                            if tells == FINISHED && watched.armed {
                                watched.armed = false;
                                finished.push(token);
                            }
                            true
                        });
                    if named && !ready.contains(&token) {
                        ready.push(token);
                    }
                }
            }
            // Where only packets let go of came, it waits on.
            if !ready.is_empty() || deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::time::{Duration, Instant};

    use super::*;

    /// A poller names what has something to read, or has ended, by its token; waits out
    /// its timeout when nothing is; names one again while anything is left to read
    /// (level); forgets what it no longer watches; and gives what one wait cannot hold in
    /// the next: `a_poller_names_what_is_ready`'s cases on Unix.
    #[test]
    fn a_poller_names_what_is_ready() {
        let poller = Poller::new().unwrap();
        let (a, a_peer) = Stream::pair().unwrap();
        let (b, b_peer) = Stream::pair().unwrap();
        poller.add(&a_peer, 7).unwrap();
        poller.add(&b_peer, 9).unwrap();
        let mut ready = Vec::new();
        let t0 = Instant::now();
        poller.wait(&mut ready, Some(Duration::from_millis(50))).unwrap();
        assert!(ready.is_empty());
        assert!(t0.elapsed() >= Duration::from_millis(50), "{:?}", t0.elapsed());
        (&a).write_all(b"x").unwrap();
        poller.wait(&mut ready, None).unwrap();
        assert_eq!(ready, [7]);
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        assert_eq!(ready, [7], "ready while unread");
        let mut byte = [0u8; 1];
        (&a_peer).read_exact(&mut byte).unwrap();
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        assert!(ready.is_empty());
        // An end is ready: a pipe has no half-close, its end is its close.
        crate::end(&b);
        drop(b);
        poller.wait(&mut ready, None).unwrap();
        assert_eq!(ready, [9]);
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        assert_eq!(ready, [9], "ended, ready again");
        poller.remove(&b_peer).unwrap();
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        assert!(ready.is_empty());
        // More ready than one wait holds: each comes, as what is read stops being ready.
        let pairs: Vec<(Stream, Stream)> = (0..150).map(|_| Stream::pair().unwrap()).collect();
        for (i, (_, theirs)) in pairs.iter().enumerate() {
            poller.add(theirs, 100 + i as u64).unwrap();
        }
        for (ours, _) in &pairs {
            (&*ours).write_all(b"y").unwrap();
        }
        let mut seen = std::collections::BTreeSet::new();
        while seen.len() < pairs.len() {
            poller.wait(&mut ready, Some(Duration::from_secs(5))).unwrap();
            assert!(!ready.is_empty(), "{} of {} seen", seen.len(), pairs.len());
            for &token in &ready {
                let (_, theirs) = &pairs[(token - 100) as usize];
                (&*theirs).read_exact(&mut byte).unwrap();
                assert!(seen.insert(token), "{token} twice");
            }
        }
    }

    /// A stream added with something to read is named by a wait under way; one whose
    /// reads are shut is named, its read cancelled, as a socket shut is readable; and one
    /// added twice is refused.
    #[test]
    fn a_poller_wakes_for_what_is_added_and_shut() {
        let poller = Poller::new().unwrap();
        let (a, a_peer) = Stream::pair().unwrap();
        let (_c, c_peer) = Stream::pair().unwrap();
        poller.add(&c_peer, 3).unwrap();
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                let mut ready = Vec::new();
                poller.wait(&mut ready, Some(Duration::from_secs(10))).unwrap();
                ready
            });
            std::thread::sleep(Duration::from_millis(50));
            (&a).write_all(b"x").unwrap();
            poller.add(&a_peer, 5).unwrap();
            assert_eq!(waiter.join().unwrap(), [5]);
        });
        assert_eq!(
            poller.add(&a_peer, 6).map_err(|e| e.kind()),
            Err(io::ErrorKind::AlreadyExists)
        );
        // 5 stays ready, unread; 3 comes as its cancelled read's wait posts it.
        let mut ready = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        c_peer.shutdown(std::net::Shutdown::Read).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen.contains(&3) {
            assert!(Instant::now() < deadline, "{seen:?}");
            poller.wait(&mut ready, Some(Duration::from_secs(10))).unwrap();
            seen.extend(ready.iter().copied());
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), [3, 5]);
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        ready.sort_unstable();
        assert_eq!(ready, [3, 5], "shut, ready again");
    }

    /// A process's end is told once it has exited, not before, and once; a process that
    /// has ended already is ready at once: `a_poller_tells_of_a_childs_end`'s cases on
    /// Unix.
    #[test]
    fn a_poller_tells_of_a_childs_end() {
        let poller = Poller::new().unwrap();
        let mut ready = Vec::new();
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = std::path::Path::new(&system).join(r"System32\cmd.exe");
        // A second's pings: the first answers at once, the second a second later.
        let mut child = std::process::Command::new(&cmd)
            .args(["/c", "ping -n 2 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        {
            let _watch = poller.add_exit(child.id(), 9).unwrap();
            poller.wait(&mut ready, Some(Duration::from_millis(50))).unwrap();
            assert!(ready.is_empty(), "told of an end to come");
            poller.wait(&mut ready, Some(Duration::from_secs(30))).unwrap();
            assert_eq!(ready, [9]);
            assert!(child.try_wait().unwrap().is_some(), "not to be taken at once");
        }
        poller.wait(&mut ready, Some(Duration::from_millis(50))).unwrap();
        assert!(ready.is_empty(), "told twice");
        // Ended and waited for: std's handle of it, held until it drops, keeps its ID its own.
        let mut ended = std::process::Command::new(&cmd)
            .args(["/c", "exit 3"])
            .spawn()
            .unwrap();
        assert_eq!(ended.wait().unwrap().code(), Some(3));
        let _watch = poller.add_exit(ended.id(), 10).unwrap();
        poller.wait(&mut ready, Some(Duration::from_secs(1))).unwrap();
        assert_eq!(ready, [10]);
    }
}
