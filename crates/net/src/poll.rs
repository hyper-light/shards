//! Waiting on the network process's descriptors (review 2.14): each registered once, its
//! interest changed only when what it waits for does, so that a wait costs what is
//! ready, not what is watched: epoll(7) on Linux, kqueue(2) on macOS. Level-triggered,
//! as poll(2) is: a descriptor stays ready while what it is ready for is left.
//!
//! A descriptor closed is forgotten by both (epoll(7) once its last reference closes,
//! kqueue(2) at close), so the caller never removes one it is closing: a change made
//! after the close could reach a new descriptor of the same number. On Windows a socket
//! closed is not forgotten, and its handle's value may name the next socket made: the
//! caller makes a socket's interest NONE before it closes it ([`Poller::set`]).
//!
//! On Windows (D142) each socket's interest selects its network events onto one event
//! (WSAEventSelect), once, as it changes; a wait reads each socket's state with a WSAPoll
//! that does not wait, and sleeps on that event and on the handles watched beside the
//! sockets (a doorbell's event, a peer's process), as the vsock device's poller does
//! (vmm vsock/poll.rs; PM M199: WSAPoll alone reports a peer's half-close as a hangup
//! whatever is asked, which a wait to write would spin on).

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
    use std::os::windows::io::RawHandle;
    use std::time::Duration;

    use windows_sys::Win32::Networking::WinSock::{
        FD_ACCEPT, FD_CLOSE, FD_CONNECT, FD_READ, FD_WRITE, POLLERR, POLLHUP, POLLNVAL, POLLRDNORM,
        POLLWRNORM, WSA_INVALID_EVENT, WSACloseEvent, WSACreateEvent, WSAEVENT, WSAEventSelect,
        WSAGetLastError, WSAPOLLFD, WSAPoll, WSAResetEvent, WSAWaitForMultipleEvents,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use super::{Event, Interest};
    use crate::sys::RawSock;

    /// A handle watched beside the sockets: under `token`, its being signaled an end where
    /// `ends` (a peer's process), and otherwise a read (a doorbell's event).
    #[derive(Debug, Clone, Copy)]
    struct Watched {
        handle: RawHandle,
        token: u64,
        ends: bool,
    }

    #[derive(Debug)]
    pub struct Poller {
        /// The event every socket's network events set.
        event: WSAEVENT,
        sockets: RefCell<HashMap<RawSock, (u64, Interest)>>,
        watched: RefCell<Vec<Watched>>,
    }

    impl Drop for Poller {
        fn drop(&mut self) {
            // SAFETY: our own event.
            unsafe { WSACloseEvent(self.event) };
        }
    }

    fn last() -> io::Error {
        // SAFETY: no arguments.
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    impl Poller {
        pub fn new() -> io::Result<Poller> {
            shards_afunix::startup()?;
            // SAFETY: no arguments; closed as the poller drops.
            let event = unsafe { WSACreateEvent() };
            if event == WSA_INVALID_EVENT {
                return Err(last());
            }
            Ok(Poller {
                event,
                sockets: RefCell::new(HashMap::new()),
                watched: RefCell::new(Vec::new()),
            })
        }

        /// Makes `sock`'s interest `now`, named `token`: NONE forgets it, as the caller
        /// does before it closes it.
        pub fn set(&self, sock: RawSock, token: u64, was: Interest, now: Interest) -> io::Result<()> {
            if was == now && now != Interest::NONE {
                return Ok(());
            }
            let mut sockets = self.sockets.borrow_mut();
            if now == Interest::NONE {
                if sockets.remove(&sock).is_some() {
                    // SAFETY: a socket of the caller's, its events selected onto nothing.
                    unsafe { WSAEventSelect(sock, 0, 0) };
                }
                return Ok(());
            }
            let mut events = 0;
            if now.read {
                events |= FD_READ | FD_ACCEPT | FD_CLOSE;
            }
            if now.write {
                events |= FD_WRITE | FD_CONNECT | FD_CLOSE;
            }
            // SAFETY: a socket of the caller's and our own event.
            if unsafe { WSAEventSelect(sock, self.event, events as i32) } != 0 {
                return Err(last());
            }
            sockets.insert(sock, (token, now));
            Ok(())
        }

        /// Watches `handle` beside the sockets, under `token`: a doorbell's event, which a
        /// wait it wakes resets; or, where `ends`, a peer's process, whose end it says.
        pub fn watch(&self, handle: RawHandle, token: u64, ends: bool) {
            self.watched.borrow_mut().push(Watched { handle, token, ends });
        }

        /// Waits until something registered is ready, or `timeout` passes (`None`: for as
        /// long as it takes), and puts what is in `out`, which it empties first.
        pub fn wait(&self, out: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<()> {
            out.clear();
            // SAFETY: our own event, set again by any network event from here on.
            unsafe { WSAResetEvent(self.event) };
            self.look(out)?;
            if !out.is_empty() || timeout == Some(Duration::ZERO) {
                return Ok(());
            }
            // Rounded up, so that a deadline is never missed by waking a moment early;
            // none waits without end (INFINITE).
            let ms = timeout.map_or(u32::MAX, |t| {
                u32::try_from(t.as_micros().div_ceil(1000)).unwrap_or(u32::MAX - 1)
            });
            let handles: Vec<RawHandle> = std::iter::once(self.event as RawHandle)
                .chain(self.watched.borrow().iter().map(|w| w.handle))
                .collect();
            // SAFETY: our own event and the handles watched, which their owner keeps open.
            unsafe { WSAWaitForMultipleEvents(handles.len() as u32, handles.as_ptr(), 0, ms, 0) };
            self.look(out)
        }

        /// What is ready now: each socket's state as a WSAPoll that does not wait reads it,
        /// end of stream (a hangup) a read's and never a write's (PM M199); and each handle
        /// watched that is signaled, a doorbell's taken as it is looked at.
        fn look(&self, out: &mut Vec<Event>) -> io::Result<()> {
            let sockets = self.sockets.borrow();
            let mut fds: Vec<WSAPOLLFD> = sockets
                .iter()
                .map(|(&fd, &(_, interest))| WSAPOLLFD {
                    fd,
                    events: if interest.read { POLLRDNORM } else { 0 }
                        | if interest.write { POLLWRNORM } else { 0 },
                    revents: 0,
                })
                .collect();
            if !fds.is_empty() {
                // SAFETY: an array of its own length; no wait.
                let n = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, 0) };
                if n < 0 {
                    return Err(last());
                }
            }
            for fd in &fds {
                let r = fd.revents;
                if r == 0 {
                    continue;
                }
                let Some(&(token, interest)) = sockets.get(&fd.fd) else {
                    continue;
                };
                let ended = r & (POLLHUP | POLLERR | POLLNVAL) != 0;
                let read = interest.read && (r & POLLRDNORM != 0 || ended);
                let write = interest.write && (r & POLLWRNORM != 0 || r & (POLLERR | POLLNVAL) != 0);
                if read || write || ended {
                    out.push(Event {
                        token,
                        read: read || ended,
                        write,
                        ended,
                    });
                }
            }
            for w in self.watched.borrow().iter() {
                // SAFETY: a handle its owner keeps open; no wait.
                if unsafe { WaitForSingleObject(w.handle, 0) } == 0 {
                    out.push(Event {
                        token: w.token,
                        read: true,
                        write: false,
                        ended: w.ends,
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
        assert_eq!(
            ready(&p),
            [Event {
                token: 7,
                read: true,
                write: false,
                ended: false
            }]
        );
        // Level-triggered: still there, still ready.
        assert_eq!(ready(&p).len(), 1);
        let both = Interest {
            read: true,
            write: true,
        };
        p.set(b.raw_sock(), 7, Interest::READ, both).unwrap();
        let events = ready(&p);
        assert!(
            events.iter().any(|e| e.write) && events.iter().any(|e| e.read),
            "{events:?}"
        );
        p.set(b.raw_sock(), 7, both, Interest::NONE).unwrap();
        assert_eq!(ready(&p), []);
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

    /// A descriptor closed is forgotten: a wait finds nothing of it. On Windows the caller
    /// forgets it first, as the stack does.
    #[test]
    fn a_closed_descriptor_is_forgotten() {
        let p = Poller::new().unwrap();
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(b"x").unwrap();
        p.set(b.raw_sock(), 9, Interest::NONE, Interest::READ).unwrap();
        #[cfg(windows)]
        p.set(b.raw_sock(), 9, Interest::READ, Interest::NONE).unwrap();
        drop(b);
        assert_eq!(ready(&p), []);
    }

    /// A handle watched is ready while signaled, under its token: a doorbell's event as a
    /// read, taken as the wait finds it; a peer's process, as its end.
    #[cfg(windows)]
    #[test]
    fn handles_watched_are_ready_when_signaled() {
        use std::os::windows::io::AsRawHandle as _;
        let p = Poller::new().unwrap();
        let (sleeps, rings) = shards_netring::doorbell().unwrap();
        p.watch(sleeps.as_raw_handle(), 5, false);
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
