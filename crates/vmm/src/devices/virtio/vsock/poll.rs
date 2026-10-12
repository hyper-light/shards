//! Waiting for host sockets to become readable or writable.
//!
//! Linux uses poll(2): for Unix sockets it reports POLLHUP only when both directions are
//! shut (net/unix/af_unix.c `unix_poll`). macOS uses kqueue(2). Its poll(2) is built on
//! kqueue and turns EV_EOF into POLLHUP, so a peer's half-close reads as a hangup. POLLHUP
//! is always reported, so a connection that only waits to write to such a socket would
//! wake without end. kqueue keeps read EOF and write readiness apart.
//!
//! Windows' WSAPoll has the same fault: a peer's half-close reads as a hangup without
//! RDNORM, reported whatever is asked (PM M199). There a wait sleeps on WSAEventSelect's
//! events, which come once a socket has something new (FD_WRITE only once room does after
//! a refused send), and each socket's state is read by a WSAPoll that does not wait.

use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::RawFd;
use std::time::Duration;

use crate::platform::RawSocket;

/// What to wait for on one descriptor.
#[derive(Debug, Clone, Copy)]
pub struct Interest<T> {
    pub fd: RawSocket,
    pub read: bool,
    pub write: bool,
    pub token: T,
}

/// What became ready: `read` covers data, EOF and errors, `write` room and errors, so
/// the next read or write reports what happened.
#[derive(Debug, Clone, Copy)]
pub struct Ready<T> {
    pub token: T,
    pub read: bool,
    pub write: bool,
}

#[cfg(target_os = "linux")]
fn millis(timeout: Option<Duration>) -> libc::c_int {
    timeout.map_or(-1, |t| {
        // Round up, so a deadline is never missed by waking a moment early.
        let ms = t.as_micros().div_ceil(1000);
        libc::c_int::try_from(ms).unwrap_or(libc::c_int::MAX)
    })
}

/// Waits for a worker's sockets. It keeps what its waits need between them: on macOS
/// one kqueue, whose registrations follow the interests (audit D06, PM M57).
pub struct Poller {
    #[cfg(target_os = "linux")]
    fds: Vec<libc::pollfd>,
    #[cfg(target_os = "macos")]
    kq: std::os::fd::OwnedFd,
    /// What the kqueue holds: (descriptor, filter), sorted.
    #[cfg(target_os = "macos")]
    registered: Vec<(RawFd, i16)>,
    #[cfg(target_os = "macos")]
    wanted: Vec<(RawFd, i16)>,
    #[cfg(target_os = "macos")]
    changes: Vec<libc::kevent>,
    #[cfg(target_os = "macos")]
    events: Vec<libc::kevent>,
    /// The event every socket's network events signal (WSAEventSelect).
    #[cfg(windows)]
    event: windows_sys::Win32::Networking::WinSock::WSAEVENT,
    #[cfg(windows)]
    fds: Vec<windows_sys::Win32::Networking::WinSock::WSAPOLLFD>,
}

#[cfg(target_os = "linux")]
impl Poller {
    pub fn new() -> io::Result<Poller> {
        Ok(Poller { fds: Vec::new() })
    }

    /// Waits until an interest is ready or `timeout` passes; appends what is ready to
    /// `out`.
    pub fn wait<T: Copy>(
        &mut self,
        interests: &[Interest<T>],
        timeout: Option<Duration>,
        out: &mut Vec<Ready<T>>,
    ) -> io::Result<()> {
        self.fds.clear();
        self.fds.extend(interests.iter().map(|i| libc::pollfd {
            fd: i.fd,
            events: (if i.read { libc::POLLIN } else { 0 }) | (if i.write { libc::POLLOUT } else { 0 }),
            revents: 0,
        }));
        // SAFETY: `fds` is a valid array of `fds.len()` entries.
        let n = unsafe {
            libc::poll(
                self.fds.as_mut_ptr(),
                self.fds.len() as libc::nfds_t,
                millis(timeout),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(e)
            };
        }
        for (f, i) in self.fds.iter().zip(interests) {
            let failed = f.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            let read = i.read && (f.revents & libc::POLLIN != 0 || failed);
            let write = i.write && (f.revents & libc::POLLOUT != 0 || failed);
            if read || write {
                out.push(Ready {
                    token: i.token,
                    read,
                    write,
                });
            }
        }
        Ok(())
    }
}

/// A change's `udata` when no interest is its: a deletion.
#[cfg(target_os = "macos")]
const NO_INTEREST: usize = usize::MAX;

#[cfg(target_os = "macos")]
impl Poller {
    pub fn new() -> io::Result<Poller> {
        use std::os::fd::FromRawFd;
        // SAFETY: kqueue(2) takes no arguments.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Poller {
            // SAFETY: a fresh descriptor nothing else owns.
            kq: unsafe { std::os::fd::OwnedFd::from_raw_fd(kq) },
            registered: Vec::new(),
            wanted: Vec::new(),
            changes: Vec::new(),
            events: Vec::new(),
        })
    }

    /// Waits until an interest is ready or `timeout` passes; appends what is ready to
    /// `out`.
    ///
    /// Every interest is added again each wait, which updates a registration it has and
    /// makes one it lacks: closing a descriptor removes its registrations (kqueue(2)), so
    /// one closed and its number reused since the last wait is registered anew, never
    /// taken for registered. What is no longer wanted is deleted, so it wakes no wait.
    pub fn wait<T: Copy>(
        &mut self,
        interests: &[Interest<T>],
        timeout: Option<Duration>,
        out: &mut Vec<Ready<T>>,
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd;

        let change = |fd: RawFd, filter: i16, flags: u16, udata: usize| libc::kevent {
            ident: fd as libc::uintptr_t,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata: udata as *mut libc::c_void,
        };
        self.wanted.clear();
        self.changes.clear();
        for (index, i) in interests.iter().enumerate() {
            for (wanted, filter) in [(i.read, libc::EVFILT_READ), (i.write, libc::EVFILT_WRITE)] {
                if wanted {
                    self.wanted.push((i.fd, filter));
                    self.changes.push(change(i.fd, filter, libc::EV_ADD, index));
                }
            }
        }
        self.wanted.sort_unstable();
        for &(fd, filter) in &self.registered {
            if self.wanted.binary_search(&(fd, filter)).is_err() {
                self.changes
                    .push(change(fd, filter, libc::EV_DELETE, NO_INTEREST));
            }
        }
        // Room for every change, and every wanted filter: a change that fails comes back
        // as an EV_ERROR event instead of ending the call (kevent(2)).
        let room = (self.changes.len() + self.wanted.len()).max(1);
        // SAFETY: an all-zero kevent is a valid value.
        self.events
            .resize(room, unsafe { std::mem::zeroed::<libc::kevent>() });
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: libc::time_t::try_from(t.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: libc::c_long::from(t.subsec_nanos()),
        });
        let ts_ptr = ts
            .as_ref()
            .map_or(std::ptr::null(), |t| t as *const libc::timespec);
        // SAFETY: both arrays are valid for their lengths; `ts_ptr` is null or a timespec.
        let n = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                self.changes.as_ptr(),
                libc::c_int::try_from(self.changes.len()).unwrap_or(libc::c_int::MAX),
                self.events.as_mut_ptr(),
                libc::c_int::try_from(room).unwrap_or(libc::c_int::MAX),
                ts_ptr,
            )
        };
        let Ok(n) = usize::try_from(n) else {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                // The changes were made before the wait was interrupted (kevent(2)).
                std::mem::swap(&mut self.registered, &mut self.wanted);
                return Ok(());
            }
            // What the kqueue holds is unknown: the next wait starts from a fresh one.
            *self = Poller::new()?;
            return Err(e);
        };
        std::mem::swap(&mut self.registered, &mut self.wanted);
        for e in self.events.iter().take(n) {
            // A deletion fails where its descriptor was closed, which removed it already.
            let Some(i) = interests.get(e.udata as usize) else {
                continue;
            };
            // A failed registration surfaces through the next read or write; it stays
            // out of what the kqueue holds only until the next wait adds it again.
            let failed = e.flags & libc::EV_ERROR != 0;
            let read = i.read && (e.filter == libc::EVFILT_READ || failed);
            let write = i.write && (e.filter == libc::EVFILT_WRITE || failed);
            if read || write {
                out.push(Ready {
                    token: i.token,
                    read,
                    write,
                });
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Poller {
    pub fn new() -> io::Result<Poller> {
        use windows_sys::Win32::Networking::WinSock::{WSA_INVALID_EVENT, WSACreateEvent};
        crate::platform::winsock()?;
        // SAFETY: no arguments.
        let event = unsafe { WSACreateEvent() };
        if event == WSA_INVALID_EVENT {
            return Err(io::Error::last_os_error());
        }
        Ok(Poller {
            event,
            fds: Vec::new(),
        })
    }

    /// Waits until an interest is ready or `timeout` passes; appends what is ready to
    /// `out`.
    ///
    /// Every interest's socket is associated with the event anew each wait, as a socket
    /// closed and its handle reused since the last is a socket the event knows nothing of.
    /// The event is reset before the sockets are looked at, so that anything after the look
    /// wakes the wait.
    pub fn wait<T: Copy>(
        &mut self,
        interests: &[Interest<T>],
        timeout: Option<Duration>,
        out: &mut Vec<Ready<T>>,
    ) -> io::Result<()> {
        use windows_sys::Win32::Networking::WinSock::{
            FD_ACCEPT, FD_CLOSE, FD_READ, FD_WRITE, WSAEventSelect, WSAResetEvent, WSAWaitForMultipleEvents,
        };
        for i in interests {
            // SAFETY: a socket of the caller's and our own event. One that fails is reported
            // by the look below (POLLNVAL).
            unsafe {
                WSAEventSelect(
                    i.fd,
                    self.event,
                    (FD_READ | FD_WRITE | FD_CLOSE | FD_ACCEPT) as i32,
                )
            };
        }
        // SAFETY: our own event.
        unsafe { WSAResetEvent(self.event) };
        let before = out.len();
        self.look(interests, out)?;
        if out.len() > before || timeout == Some(Duration::ZERO) {
            return Ok(());
        }
        // Rounded up, so a deadline is never missed by waking a moment early; none waits
        // without end (INFINITE).
        let ms = timeout.map_or(u32::MAX, |t| {
            u32::try_from(t.as_micros().div_ceil(1000)).unwrap_or(u32::MAX - 1)
        });
        let handle = self.event as windows_sys::Win32::Foundation::HANDLE;
        // SAFETY: one event handle of ours.
        unsafe { WSAWaitForMultipleEvents(1, &raw const handle, 0, ms, 0) };
        self.look(interests, out)
    }

    /// What is ready of `interests` now: a WSAPoll that does not wait. End-of-stream is a
    /// hangup (POLLHUP) here, and a read's alone, never a reason to wake a writer.
    fn look<T: Copy>(&mut self, interests: &[Interest<T>], out: &mut Vec<Ready<T>>) -> io::Result<()> {
        use windows_sys::Win32::Networking::WinSock::{
            POLLERR, POLLHUP, POLLNVAL, POLLRDNORM, POLLWRNORM, WSAPOLLFD, WSAPoll,
        };
        self.fds.clear();
        self.fds.extend(interests.iter().map(|i| WSAPOLLFD {
            fd: i.fd,
            events: (if i.read { POLLRDNORM } else { 0 }) | (if i.write { POLLWRNORM } else { 0 }),
            revents: 0,
        }));
        if self.fds.is_empty() {
            return Ok(());
        }
        // SAFETY: `fds` is a valid array of `fds.len()` entries; no wait.
        let n = unsafe { WSAPoll(self.fds.as_mut_ptr(), self.fds.len() as u32, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        for (f, i) in self.fds.iter().zip(interests) {
            let failed = f.revents & (POLLERR | POLLNVAL) != 0;
            let read = i.read && (f.revents & (POLLRDNORM | POLLHUP) != 0 || failed);
            let write = i.write && (f.revents & POLLWRNORM != 0 || failed);
            if read || write {
                out.push(Ready {
                    token: i.token,
                    read,
                    write,
                });
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for Poller {
    fn drop(&mut self) {
        // SAFETY: our own event, which no wait uses once the poller drops.
        unsafe { windows_sys::Win32::Networking::WinSock::WSACloseEvent(self.event) };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write as _;
    use std::net::Shutdown;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::platform::{AsRawSocket, UnixStream};

    fn ready(i: Interest<u8>) -> Vec<Ready<u8>> {
        let mut out = Vec::new();
        Poller::new()
            .unwrap()
            .wait(&[i], Some(Duration::from_millis(50)), &mut out)
            .unwrap();
        out
    }

    #[cfg(unix)]
    fn interest(fd: RawSocket, read: bool, write: bool, token: u8) -> Interest<u8> {
        Interest {
            fd,
            read,
            write,
            token,
        }
    }

    /// One poller's waits: an interest dropped wakes none, one whose descriptor was
    /// closed and its number reused is the new descriptor's, and one waited for again
    /// wakes again, under its new token.
    #[cfg(unix)]
    #[test]
    fn a_pollers_waits_follow_their_interests() {
        let mut poller = Poller::new().unwrap();
        let mut out = Vec::new();
        let short = Some(Duration::from_millis(20));
        let (ours, theirs) = UnixStream::pair().unwrap();
        let fd = ours.as_raw_fd();
        // Writable, and wanted so.
        poller
            .wait(&[interest(fd, true, true, 1)], short, &mut out)
            .unwrap();
        assert!(out.iter().any(|r| r.token == 1 && r.write && !r.read), "{out:?}");
        // No longer wanted writable, and nothing to read: no wake. A wait nothing wakes
        // lasts its whole timeout; one a leftover registration wakes returns at once,
        // with nothing to report.
        out.clear();
        let waited = std::time::Instant::now();
        poller
            .wait(&[interest(fd, true, false, 2)], short, &mut out)
            .unwrap();
        assert!(out.is_empty(), "{out:?}");
        assert!(
            waited.elapsed() >= Duration::from_millis(20),
            "a dropped interest woke a wait after {:?}",
            waited.elapsed()
        );
        // Readable once the peer writes, under the token of the wait.
        (&theirs).write_all(b"x").unwrap();
        poller
            .wait(&[interest(fd, true, false, 3)], short, &mut out)
            .unwrap();
        assert!(out.iter().any(|r| r.token == 3 && r.read), "{out:?}");
        // Closed, and its number taken by a new socket with data for us. dup2(2) does
        // both at once: closing first would leave the number to any test thread that
        // opens a descriptor in between.
        let (new, peer) = UnixStream::pair().unwrap();
        // SAFETY: `fd` is `ours`, which owns the new socket from here on.
        assert_eq!(unsafe { libc::dup2(new.as_raw_fd(), fd) }, fd);
        drop((new, theirs));
        (&peer).write_all(b"y").unwrap();
        out.clear();
        poller
            .wait(&[interest(fd, true, false, 4)], short, &mut out)
            .unwrap();
        assert!(
            out.iter().any(|r| r.token == 4 && r.read),
            "a reused number went unwatched: {out:?}"
        );
    }

    #[test]
    fn a_half_closed_peer_is_readable_but_not_a_reason_to_wake_a_writer() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        // Fill our send buffer so the socket is not writable.
        let chunk = [0u8; 4096];
        while (&ours).write(&chunk).is_ok() {}
        theirs.shutdown(Shutdown::Write).unwrap();
        let fd = ours.raw_socket();
        let r = ready(Interest {
            fd,
            read: true,
            write: false,
            token: 1,
        });
        assert!(r.first().is_some_and(|r| r.read), "EOF is readable");
        let w = ready(Interest {
            fd,
            read: false,
            write: true,
            token: 2,
        });
        assert!(w.is_empty(), "a writer woke for a half-close: {w:?}");
    }
}
