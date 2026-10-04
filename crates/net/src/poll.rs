//! Waiting on the network process's descriptors (review 2.14): each registered once, its
//! interest changed only when what it waits for does, so that a wait costs what is
//! ready, not what is watched: epoll(7) on Linux, kqueue(2) on macOS. Level-triggered,
//! as poll(2) is: a descriptor stays ready while what it is ready for is left.
//!
//! A descriptor closed is forgotten by both (epoll(7) once its last reference closes,
//! kqueue(2) at close), so the caller never removes one it is closing: a change made
//! after the close could reach a new descriptor of the same number.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

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
const AT_ONCE: usize = 256;

#[derive(Debug)]
pub struct Poller {
    fd: OwnedFd,
}

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
    pub fn set(&self, fd: RawFd, token: u64, was: Interest, now: Interest) -> io::Result<()> {
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
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

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
        p.set(b.as_raw_fd(), 7, Interest::NONE, Interest::READ).unwrap();
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
        p.set(b.as_raw_fd(), 7, Interest::READ, both).unwrap();
        let events = ready(&p);
        assert!(
            events.iter().any(|e| e.write) && events.iter().any(|e| e.read),
            "{events:?}"
        );
        p.set(b.as_raw_fd(), 7, both, Interest::NONE).unwrap();
        assert_eq!(ready(&p), []);
        let mut got = [0u8; 1];
        (&b).read_exact(&mut got).unwrap();
    }

    /// A peer's end is said, as the next read finds it.
    #[test]
    fn an_end_is_said() {
        let p = Poller::new().unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        p.set(b.as_raw_fd(), 3, Interest::NONE, Interest::READ).unwrap();
        drop(a);
        let events = ready(&p);
        assert_eq!(events.len(), 1);
        assert!(events[0].read && events[0].ended, "{events:?}");
    }

    /// A descriptor closed is forgotten: a wait finds nothing of it.
    #[test]
    fn a_closed_descriptor_is_forgotten() {
        let p = Poller::new().unwrap();
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(b"x").unwrap();
        p.set(b.as_raw_fd(), 9, Interest::NONE, Interest::READ).unwrap();
        drop(b);
        assert_eq!(ready(&p), []);
    }
}
