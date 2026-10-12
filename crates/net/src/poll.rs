//! Waiting on the network process's descriptors (review 2.14): each registered once, its
//! interest changed only when what it waits for does, so that a wait costs what is
//! ready, not what is watched: epoll(7) on Linux, kqueue(2) on macOS, and on Windows a
//! completion port. Level-triggered, as poll(2) is: a descriptor stays ready while what
//! it is ready for is left.
//!
//! A descriptor closed is forgotten by both (epoll(7) once its last reference closes,
//! kqueue(2) at close), so a change made after the close could reach a new descriptor of
//! the same number: the caller forgets one before it closes it ([`Poller::forget`]). On
//! Windows a closed socket's registration goes with it, but notices already queued for it
//! may still come, and its handle's value may name the next socket made: forgotten first,
//! its notices are let go of as they come.
//!
//! On Windows (D142) each socket is registered with a completion port as its interest
//! changes (ProcessSocketNotifications, level-triggered), and a wait takes the port's
//! packets, which name the sockets ready: one event every socket set, and a look at every
//! socket after each wake, cost a round trip 39 to 54 ms beside 10,000 idle sockets,
//! where the port costs what it does beside none (PM M213). A handle watched beside the
//! sockets (a peer's process) comes as a packet a thread-pool wait posts. A hangup is a
//! read's and never a write's (PM M199: a peer's half-close is a hangup whatever is
//! asked, which a wait to write would spin on). A socket that waits for nothing for a
//! while is registered again filtering no events, not removed: a removal ends only with
//! its notice, and a registration enabled again before it came failed, which reset the
//! socket's connection (PM M215).

#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use crate::sys::RawSock;

/// What a descriptor waits for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Interest {
    pub read: bool,
    pub write: bool,
}

impl Interest {
    pub const NONE: Interest = Interest {
        read: false,
        write: false,
    };
    pub const READ: Interest = Interest {
        read: true,
        write: false,
    };
}

/// A descriptor ready: for reading, writing, or with its end or an error, which the next
/// read or write says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub token: u64,
    pub read: bool,
    pub write: bool,
    pub ended: bool,
}

/// Events a wait takes at most; more wait for the next.
#[cfg(unix)]
const AT_ONCE: usize = 256;

#[cfg(unix)]
#[derive(Debug)]
pub struct Poller {
    fd: OwnedFd,
}

#[cfg(unix)]
impl Poller {
    pub fn new() -> io::Result<Poller> {
        #[cfg(target_os = "macos")]
        // SAFETY: kqueue(2) takes no arguments; a child inherits no kqueue.
        let raw = unsafe { libc::kqueue() };
        #[cfg(not(target_os = "macos"))]
        // SAFETY: epoll_create1(2) with a flag.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just made, owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Poller { fd })
    }

    /// Forgets `fd`, waited on under `token` for `had`, before the caller closes it.
    pub fn forget(&self, fd: RawSock, token: u64, had: Interest) -> io::Result<()> {
        self.set(fd, token, had, Interest::NONE)
    }

    /// Makes `fd`'s interest `now`, named `token`, from `was`, what it was (NONE for one
    /// not registered).
    pub fn set(&self, fd: RawSock, token: u64, was: Interest, now: Interest) -> io::Result<()> {
        if was == now {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kevent is plain data, for which all zeroes is a value.
            let mut changes: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
            let mut n = 0;
            for (filter, before, after) in [
                (libc::EVFILT_READ, was.read, now.read),
                (libc::EVFILT_WRITE, was.write, now.write),
            ] {
                if before == after {
                    continue;
                }
                if let Some(c) = changes.get_mut(n) {
                    c.ident = fd as libc::uintptr_t;
                    c.filter = filter;
                    c.flags = if after { libc::EV_ADD } else { libc::EV_DELETE };
                    c.udata = token as usize as *mut libc::c_void;
                    n += 1;
                }
            }
            // SAFETY: kevent(2) applying `n` changes, asking for no events.
            if unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    changes.as_ptr(),
                    n as libc::c_int,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let op = if was == Interest::NONE {
                libc::EPOLL_CTL_ADD
            } else if now == Interest::NONE {
                libc::EPOLL_CTL_DEL
            } else {
                libc::EPOLL_CTL_MOD
            };
            let mut event = libc::epoll_event {
                events: (if now.read {
                    libc::EPOLLIN | libc::EPOLLRDHUP
                } else {
                    0
                } | if now.write { libc::EPOLLOUT } else { 0 }) as u32,
                u64: token,
            };
            // SAFETY: epoll_ctl(2) on a descriptor the caller holds open.
            if unsafe { libc::epoll_ctl(self.fd.as_raw_fd(), op, fd, &mut event) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    /// Waits until something registered is ready, or `timeout` passes (`None`: for as
    /// long as it takes), and puts what is in `out`, which it empties first. A signal's
    /// interruption is a wait that found nothing.
    pub fn wait(&self, out: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<()> {
        out.clear();
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kevent is plain data, for which all zeroes is a value.
            let mut events: [libc::kevent; AT_ONCE] = unsafe { std::mem::zeroed() };
            let spec = timeout.map(|t| libc::timespec {
                tv_sec: libc::time_t::try_from(t.as_secs()).unwrap_or(libc::time_t::MAX),
                tv_nsec: libc::c_long::from(t.subsec_nanos()),
            });
            // SAFETY: kevent(2) asking for up to AT_ONCE events into a buffer that size.
            let n = unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    AT_ONCE as libc::c_int,
                    spec.as_ref()
                        .map_or(std::ptr::null(), |s| s as *const libc::timespec),
                )
            };
            let n = match usize::try_from(n) {
                Ok(n) => n,
                Err(_) => return interrupted_or(io::Error::last_os_error()),
            };
            out.extend(events.iter().take(n).map(|e| {
                let ended = e.flags & (libc::EV_EOF | libc::EV_ERROR) != 0;
                Event {
                    token: e.udata as usize as u64,
                    read: e.filter == libc::EVFILT_READ || ended,
                    write: e.filter == libc::EVFILT_WRITE || ended,
                    ended,
                }
            }));
        }
        #[cfg(not(target_os = "macos"))]
        {
            // SAFETY: epoll_event is plain data, for which all zeroes is a value.
            let mut events: [libc::epoll_event; AT_ONCE] = unsafe { std::mem::zeroed() };
            // Rounded up: a wait shorter than asked would come back before its deadline.
            let ms = timeout.map_or(-1, |t| {
                libc::c_int::try_from(t.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
            });
            // SAFETY: epoll_wait(2) for up to AT_ONCE events into a buffer that size.
            let n = unsafe {
                libc::epoll_wait(
                    self.fd.as_raw_fd(),
                    events.as_mut_ptr(),
                    AT_ONCE as libc::c_int,
                    ms,
                )
            };
            let n = match usize::try_from(n) {
                Ok(n) => n,
                Err(_) => return interrupted_or(io::Error::last_os_error()),
            };
            out.extend(events.iter().take(n).map(|e| {
                let flags = e.events;
                let ended = flags & (libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLRDHUP) as u32 != 0;
                Event {
                    token: e.u64,
                    read: flags & libc::EPOLLIN as u32 != 0 || ended,
                    write: flags & libc::EPOLLOUT as u32 != 0 || ended,
                    ended,
                }
            }));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
    use windows_sys::Win32::Networking::WinSock::{
        ProcessSocketNotifications, SOCK_NOTIFY_EVENT_ERR, SOCK_NOTIFY_EVENT_HANGUP, SOCK_NOTIFY_EVENT_IN,
        SOCK_NOTIFY_EVENT_OUT, SOCK_NOTIFY_EVENT_REMOVE, SOCK_NOTIFY_OP_ENABLE, SOCK_NOTIFY_OP_REMOVE,
        SOCK_NOTIFY_REGISTER_EVENT_HANGUP, SOCK_NOTIFY_REGISTER_EVENT_IN, SOCK_NOTIFY_REGISTER_EVENT_NONE,
        SOCK_NOTIFY_REGISTER_EVENT_OUT, SOCK_NOTIFY_REGISTRATION, SOCK_NOTIFY_TRIGGER_LEVEL,
        SOCK_NOTIFY_TRIGGER_PERSISTENT,
    };
    use windows_sys::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
    };
    use windows_sys::Win32::System::Threading::{
        INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEINWAITTHREAD, WT_EXECUTEONLYONCE,
    };

    use super::{Event, Interest};
    use crate::sys::RawSock;

    /// What a watched handle's packets carry for their bytes, which no socket's
    /// notification does: its events are bits below 256.
    const WATCHED: u32 = 1 << 16;

    /// Packets a wait takes at most; more wait for the next.
    const AT_ONCE: usize = 256;

    /// What a watched handle's wait posts to the port, kept where the wait can reach it
    /// until the wait is gone.
    struct Post {
        port: usize,
        key: usize,
    }

    /// A handle watched beside the sockets: the thread-pool wait that posts its signal to
    /// the port (RegisterWaitForSingleObject, PM M213), under `token`, an end where `ends`
    /// (a peer's process), a read otherwise (a doorbell's event).
    struct Watched {
        wait: HANDLE,
        token: u64,
        ends: bool,
        _post: Box<Post>,
    }

    /// A completion port its sockets are registered with as their interest changes
    /// (ProcessSocketNotifications, level-triggered), which names those ready: a wait
    /// costs what is ready, not what is watched (PM M213: a round trip beside 10,000 idle
    /// sockets 37 to 41 µs, where a look at each cost 39 to 54 ms).
    pub struct Poller {
        port: OwnedHandle,
        sockets: RefCell<HashMap<RawSock, (u64, Interest)>>,
        /// Each socket's interest by its token, which its notifications name.
        interests: RefCell<HashMap<u64, Interest>>,
        watched: RefCell<Vec<Watched>>,
    }

    impl std::fmt::Debug for Poller {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Poller")
                .field("sockets", &self.sockets.borrow().len())
                .field("watched", &self.watched.borrow().len())
                .finish()
        }
    }

    impl Drop for Poller {
        fn drop(&mut self) {
            for w in self.watched.borrow_mut().drain(..) {
                // SAFETY: a wait of ours, ended, its callbacks done, before what it posts
                // goes with it.
                unsafe { UnregisterWaitEx(w.wait, INVALID_HANDLE_VALUE) };
            }
        }
    }

    /// Posts a watched handle's signal to its port.
    unsafe extern "system" fn posted(context: *mut core::ffi::c_void, _: bool) {
        // SAFETY: the Post the wait was registered with, which outlives it.
        let post = unsafe { &*context.cast::<Post>() };
        // SAFETY: a port the poller holds open while the wait lives; no overlapped.
        unsafe { PostQueuedCompletionStatus(post.port as HANDLE, WATCHED, post.key, std::ptr::null()) };
    }

    impl Poller {
        pub fn new() -> io::Result<Poller> {
            shards_afunix::startup()?;
            // SAFETY: a new port, owned below.
            let raw = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Poller {
                // SAFETY: a handle just made, which nothing else owns.
                port: unsafe { OwnedHandle::from_raw_handle(raw) },
                sockets: RefCell::new(HashMap::new()),
                interests: RefCell::new(HashMap::new()),
                watched: RefCell::new(Vec::new()),
            })
        }

        /// Registers `sock` with the port under `token` for `filter`, or removes it.
        fn register(&self, sock: RawSock, token: u64, filter: u32, operation: u32) -> io::Result<()> {
            let key = usize::try_from(token).map_err(|_| io::Error::other("a token past a key"))?;
            let mut reg = SOCK_NOTIFY_REGISTRATION {
                socket: sock,
                completionKey: std::ptr::without_provenance_mut(key),
                eventFilter: u16::try_from(filter).map_err(|_| io::Error::other("an event filter"))?,
                operation: u8::try_from(operation).map_err(|_| io::Error::other("an operation"))?,
                triggerFlags: u8::try_from(SOCK_NOTIFY_TRIGGER_PERSISTENT | SOCK_NOTIFY_TRIGGER_LEVEL)
                    .map_err(|_| io::Error::other("a trigger"))?,
                registrationResult: 0,
            };
            // SAFETY: one registration of our port's; no wait, no packet taken.
            let r = unsafe {
                ProcessSocketNotifications(
                    self.port.as_raw_handle(),
                    1,
                    &raw mut reg,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if r != 0 && r != WAIT_TIMEOUT {
                return Err(io::Error::from_raw_os_error(r as i32));
            }
            if reg.registrationResult != 0 {
                return Err(io::Error::from_raw_os_error(reg.registrationResult as i32));
            }
            Ok(())
        }

        /// Makes `sock`'s interest `now`, named `token`. NONE keeps its registration,
        /// filtering no events, which a registration made again overwrites: one removed
        /// would be enabled again only once its removal's notice came (PM M215).
        pub fn set(&self, sock: RawSock, token: u64, was: Interest, now: Interest) -> io::Result<()> {
            if was == now {
                return Ok(());
            }
            let mut filter = SOCK_NOTIFY_REGISTER_EVENT_NONE;
            if now != Interest::NONE {
                filter |= SOCK_NOTIFY_REGISTER_EVENT_HANGUP;
            }
            if now.read {
                filter |= SOCK_NOTIFY_REGISTER_EVENT_IN;
            }
            if now.write {
                filter |= SOCK_NOTIFY_REGISTER_EVENT_OUT;
            }
            self.register(sock, token, filter, SOCK_NOTIFY_OP_ENABLE)?;
            if let Some((old, _)) = self.sockets.borrow_mut().insert(sock, (token, now))
                && old != token
            {
                self.interests.borrow_mut().remove(&old);
            }
            // What waits for nothing is told nothing, its notices queued before let go of.
            let mut interests = self.interests.borrow_mut();
            if now == Interest::NONE {
                interests.remove(&token);
            } else {
                interests.insert(token, now);
            }
            Ok(())
        }

        /// Forgets `sock` before the caller closes it: its registration removed, and its
        /// notices let go of as they come, its removal's own among them.
        pub fn forget(&self, sock: RawSock, _token: u64, _had: Interest) -> io::Result<()> {
            if let Some((token, _)) = self.sockets.borrow_mut().remove(&sock) {
                self.interests.borrow_mut().remove(&token);
                self.register(sock, token, 0, SOCK_NOTIFY_OP_REMOVE)?;
            }
            Ok(())
        }

        /// Watches `handle` beside the sockets, under `token`: a doorbell's event, which a
        /// wait it wakes resets; or, where `ends`, a peer's process, whose end it says,
        /// once. Its signal comes as a packet a thread-pool wait posts (PM M213).
        pub fn watch(&self, handle: RawHandle, token: u64, ends: bool) -> io::Result<()> {
            let key = usize::try_from(token).map_err(|_| io::Error::other("a token past a key"))?;
            let post = Box::new(Post {
                port: self.port.as_raw_handle() as usize,
                key,
            });
            let mut wait: HANDLE = std::ptr::null_mut();
            let once = if ends { WT_EXECUTEONLYONCE } else { 0 };
            // SAFETY: a handle its owner keeps open while it is watched; the Post outlives
            // the wait, which Drop ends first.
            let ok = unsafe {
                RegisterWaitForSingleObject(
                    &raw mut wait,
                    handle,
                    Some(posted),
                    std::ptr::from_ref::<Post>(&post).cast(),
                    INFINITE,
                    WT_EXECUTEINWAITTHREAD | once,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            self.watched.borrow_mut().push(Watched {
                wait,
                token,
                ends,
                _post: post,
            });
            Ok(())
        }

        /// Waits until something registered is ready, or `timeout` passes (`None`: for as
        /// long as it takes), and puts what is in `out`, which it empties first.
        pub fn wait(&self, out: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<()> {
            out.clear();
            // Rounded up, so that a deadline is never missed by waking a moment early;
            // none waits without end (INFINITE).
            let ms = timeout.map_or(INFINITE, |t| {
                u32::try_from(t.as_micros().div_ceil(1000)).unwrap_or(INFINITE - 1)
            });
            let mut entries = [OVERLAPPED_ENTRY::default(); AT_ONCE];
            let mut got = 0u32;
            // SAFETY: our port and an array of its own length.
            let ok = unsafe {
                GetQueuedCompletionStatusEx(
                    self.port.as_raw_handle(),
                    entries.as_mut_ptr(),
                    AT_ONCE as u32,
                    &raw mut got,
                    ms,
                    0,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                    return Ok(());
                }
                return Err(e);
            }
            let interests = self.interests.borrow();
            let watched = self.watched.borrow();
            for entry in entries.iter().take(got as usize) {
                let token = entry.lpCompletionKey as u64;
                let bits = entry.dwNumberOfBytesTransferred;
                if bits & WATCHED != 0 {
                    if let Some(w) = watched.iter().find(|w| w.token == token) {
                        out.push(Event {
                            token,
                            read: true,
                            write: false,
                            ended: w.ends,
                        });
                    }
                    continue;
                }
                // A registration removed, or one changed since its notice was queued.
                if bits & SOCK_NOTIFY_EVENT_REMOVE != 0 {
                    continue;
                }
                let Some(&interest) = interests.get(&token) else {
                    continue;
                };
                // End of stream (a hangup) a read's and never a write's (PM M199).
                let ended = bits & (SOCK_NOTIFY_EVENT_HANGUP | SOCK_NOTIFY_EVENT_ERR) != 0;
                let read = interest.read && (bits & SOCK_NOTIFY_EVENT_IN != 0 || ended);
                let write = interest.write && bits & (SOCK_NOTIFY_EVENT_OUT | SOCK_NOTIFY_EVENT_ERR) != 0;
                if read || write || ended {
                    out.push(Event {
                        token,
                        read: read || ended,
                        write,
                        ended,
                    });
                }
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use windows::Poller;

#[cfg(unix)]
fn interrupted_or(e: io::Error) -> io::Result<()> {
    if e.kind() == io::ErrorKind::Interrupted {
        Ok(())
    } else {
        Err(e)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sys::{AsRawSock as _, UnixStream};
    use std::io::{Read as _, Write as _};
    use std::time::Duration;

    fn ready(p: &Poller) -> Vec<Event> {
        let mut out = Vec::new();
        p.wait(&mut out, Some(Duration::ZERO)).unwrap();
        out.sort_by_key(|e| e.token);
        out
    }

    /// The events of waits until one is ready to read and one to write, five seconds at
    /// most: Windows queues a registration's notices as it is made, not within the call
    /// that makes it.
    fn until_both(p: &Poller) -> Vec<Event> {
        let mut seen = Vec::new();
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !(seen.iter().any(|e: &Event| e.read) && seen.iter().any(|e: &Event| e.write)) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let mut out = Vec::new();
            p.wait(&mut out, Some(left)).unwrap();
            seen.extend(out);
        }
        seen
    }

    /// A descriptor is ready for what it waits for, under its token, while that is left:
    /// to read while bytes are, to write while there is room; its interest changes as
    /// asked, and one waiting for nothing wakes nothing.
    #[test]
    fn descriptors_are_ready_for_what_they_wait_for() {
        let p = Poller::new().unwrap();
        let (mut a, b) = UnixStream::pair().unwrap();
        p.set(b.raw_sock(), 7, Interest::NONE, Interest::READ).unwrap();
        assert_eq!(ready(&p), []);
        a.write_all(b"x").unwrap();
        let mut out = Vec::new();
        p.wait(&mut out, Some(Duration::from_secs(5))).unwrap();
        assert_eq!(
            out,
            [Event {
                token: 7,
                read: true,
                write: false,
                ended: false
            }]
        );
        // Level-triggered: still there, still ready.
        p.wait(&mut out, Some(Duration::from_secs(5))).unwrap();
        assert_eq!(out.len(), 1);
        let both = Interest {
            read: true,
            write: true,
        };
        p.set(b.raw_sock(), 7, Interest::READ, both).unwrap();
        let events = until_both(&p);
        assert!(
            events.iter().all(|e| e.token == 7)
                && events.iter().any(|e| e.write)
                && events.iter().any(|e| e.read),
            "{events:?}"
        );
        p.set(b.raw_sock(), 7, both, Interest::NONE).unwrap();
        // What was queued before goes with the interest.
        p.wait(&mut out, Some(Duration::from_millis(5))).unwrap();
        assert_eq!(out, []);
        let mut got = [0u8; 1];
        (&b).read_exact(&mut got).unwrap();
    }

    /// A peer's end is said, as the next read finds it.
    #[test]
    fn an_end_is_said() {
        let p = Poller::new().unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        p.set(b.raw_sock(), 3, Interest::NONE, Interest::READ).unwrap();
        drop(a);
        let events = ready(&p);
        assert_eq!(events.len(), 1);
        assert!(events[0].read && events[0].ended, "{events:?}");
    }

    /// A descriptor forgotten, then closed, is gone: a wait finds nothing of it, its
    /// notices queued before let go of.
    #[test]
    fn a_closed_descriptor_is_forgotten() {
        let p = Poller::new().unwrap();
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(b"x").unwrap();
        p.set(b.raw_sock(), 9, Interest::NONE, Interest::READ).unwrap();
        p.forget(b.raw_sock(), 9, Interest::READ).unwrap();
        drop(b);
        assert_eq!(ready(&p), []);
    }

    /// A descriptor that waits for nothing for a while is told nothing, and what it waits
    /// for next it is told, again and again: as a connection waits whose guest's window
    /// is full and which has nothing for its host. On Windows a registration removed and
    /// enabled again at once failed, which reset the connection (PM M215).
    #[test]
    fn a_descriptor_waits_for_nothing_then_again() {
        let p = Poller::new().unwrap();
        let (mut a, b) = UnixStream::pair().unwrap();
        for round in 0..200 {
            p.set(b.raw_sock(), 4, Interest::NONE, Interest::READ).unwrap();
            p.set(b.raw_sock(), 4, Interest::READ, Interest::NONE).unwrap();
            a.write_all(b"x").unwrap();
            let mut out = Vec::new();
            p.wait(&mut out, Some(Duration::from_millis(5))).unwrap();
            assert_eq!(out, [], "round {round}: told while waiting for nothing");
            p.set(b.raw_sock(), 4, Interest::NONE, Interest::READ).unwrap();
            p.wait(&mut out, Some(Duration::from_secs(5))).unwrap();
            assert_eq!(
                out,
                [Event {
                    token: 4,
                    read: true,
                    write: false,
                    ended: false
                }],
                "round {round}"
            );
            let mut got = [0u8; 1];
            (&b).read_exact(&mut got).unwrap();
            p.set(b.raw_sock(), 4, Interest::READ, Interest::NONE).unwrap();
        }
        p.forget(b.raw_sock(), 4, Interest::NONE).unwrap();
    }

    /// A handle watched is ready while signaled, under its token: a doorbell's event as a
    /// read, taken as the wait finds it; a peer's process, as its end.
    #[cfg(windows)]
    #[test]
    fn handles_watched_are_ready_when_signaled() {
        use std::os::windows::io::AsRawHandle as _;
        let p = Poller::new().unwrap();
        let (shards_netring::Bell::Event(sleeps), shards_netring::Bell::Event(rings)) =
            shards_netring::doorbell().unwrap()
        else {
            panic!("a doorbell of events");
        };
        p.watch(sleeps.as_raw_handle(), 5, false).unwrap();
        assert_eq!(ready(&p), []);
        // SAFETY: an event of the test's.
        unsafe { windows_sys::Win32::System::Threading::SetEvent(rings.as_raw_handle()) };
        let mut out = Vec::new();
        p.wait(&mut out, Some(Duration::from_secs(5))).unwrap();
        assert_eq!(
            out,
            [Event {
                token: 5,
                read: true,
                write: false,
                ended: false
            }]
        );
        assert_eq!(ready(&p), [], "a doorbell's ring is taken as it is found");
    }
}
