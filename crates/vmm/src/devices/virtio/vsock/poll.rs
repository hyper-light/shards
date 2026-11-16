//! Waiting for host sockets to become readable or writable.
//!
//! Linux uses poll(2): for Unix sockets it reports POLLHUP only when both directions are
//! shut (net/unix/af_unix.c `unix_poll`). macOS uses kqueue(2). Its poll(2) is built on
//! kqueue and turns EV_EOF into POLLHUP, so a peer's half-close reads as a hangup. POLLHUP
//! is always reported, so a connection that only waits to write to such a socket would
//! wake without end. kqueue keeps read EOF and write readiness apart.
//!
//! Windows' WSAPoll has the same fault: a peer's half-close reads as a hangup without
//! RDNORM, reported whatever is asked (PM M199). There a wait takes the packets of the
//! device's completion port (D272): its waker's, posted; each host program's pipes', which
//! tell the port of their calls' ends under their pair's key, never reused; and the served
//! ports' sockets', whose WSAEventSelect events a thread-pool wait posts as they come
//! (FD_WRITE only once room does after a refused send), each socket's state then read by a
//! WSAPoll that does not wait. A packet only says to look: each wait's readiness is taken
//! before it waits (the pipes' by their owner, [`Source::Pipes`]), as a port tells of each
//! end once.

use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::RawFd;
use std::time::Duration;

use crate::platform::RawSocket;

/// What an interest waits on: a socket.
#[cfg(unix)]
pub type Source = RawSocket;

/// What an interest waits on on Windows (D272).
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// A socket, ready as its network events and WSAPoll say.
    Socket(RawSocket),
    /// A host program's pipes, or a listener's pair: the key their handles tell the device's
    /// completion port under, where they were associated with one (none: looked at each
    /// round), and whether they were ready as their owner looked at them before the wait.
    Pipes { key: Option<usize>, ready: bool },
}

/// What a wait on socket `fd` waits on.
#[cfg(unix)]
pub fn socket(fd: RawSocket) -> Source {
    fd
}

/// What a wait on socket `fd` waits on.
#[cfg(windows)]
pub fn socket(fd: RawSocket) -> Source {
    Source::Socket(fd)
}

/// The completion key the device's waker posts its wakes under (Windows).
#[cfg(windows)]
pub const WAKER: usize = 0;
/// The key the sockets' thread-pool wait posts under as their event is set.
#[cfg(windows)]
const SOCKETS: usize = 1;
/// The first key a pair's handles are given (the muxer's `next_key`): past the waker's and
/// the sockets'.
#[cfg(windows)]
pub const FIRST_KEY: usize = 2;

/// A completion port, of one thread: the device's worker alone takes its packets.
#[cfg(windows)]
pub fn completion_port() -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::IO::CreateIoCompletionPort;
    // SAFETY: a new port, owned below.
    let raw = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle just made, which nothing else owns.
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) })
}

/// What to wait for on one descriptor.
#[derive(Debug, Clone, Copy)]
pub struct Interest<T> {
    pub fd: Source,
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
    /// The thread-pool wait that posts [`SOCKETS`] to the port as `event` is set: before the
    /// port and the event, which it uses until it goes. Made as a wait first has a socket.
    #[cfg(windows)]
    sockets: Option<SocketWait>,
    /// The port its waits take packets from: its own (`own`), or the device's waker's
    /// ([`Poller::watch`]), which its owner keeps open while the poller waits.
    #[cfg(windows)]
    port: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    own: Option<std::os::windows::io::OwnedHandle>,
    /// The event every socket's network events signal (WSAEventSelect).
    #[cfg(windows)]
    event: windows_sys::Win32::Networking::WinSock::WSAEVENT,
    #[cfg(windows)]
    fds: Vec<windows_sys::Win32::Networking::WinSock::WSAPOLLFD>,
    #[cfg(windows)]
    entries: Vec<windows_sys::Win32::System::IO::OVERLAPPED_ENTRY>,
    /// Each pipes' interest by its key, for a wait's packets: an interest's index.
    #[cfg(windows)]
    keyed: std::collections::HashMap<usize, usize>,
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

/// A thread-pool wait on the sockets' event that posts [`SOCKETS`] to the port each time it
/// is set and the event signalled, as shards_ipc's poller's waits post its streams'.
#[cfg(windows)]
struct SocketWait {
    wait: windows_sys::Win32::System::Threading::PTP_WAIT,
}

/// Posts the sockets' packet to the port, its wait's context.
#[cfg(windows)]
unsafe extern "system" fn posted(
    _: windows_sys::Win32::System::Threading::PTP_CALLBACK_INSTANCE,
    port: *mut core::ffi::c_void,
    _: windows_sys::Win32::System::Threading::PTP_WAIT,
    _: u32,
) {
    // SAFETY: the port the wait was made with, which its poller keeps open while the wait
    // lives (`Poller::drop`, `watch`); no OVERLAPPED.
    unsafe { windows_sys::Win32::System::IO::PostQueuedCompletionStatus(port, 0, SOCKETS, std::ptr::null()) };
}

#[cfg(windows)]
impl SocketWait {
    /// A wait that posts to `port`, which outlives it.
    fn new(port: windows_sys::Win32::Foundation::HANDLE) -> io::Result<SocketWait> {
        use windows_sys::Win32::System::Threading::CreateThreadpoolWait;
        // SAFETY: a wait whose callback posts to `port`, its context, which its poller keeps
        // open as long as the wait lives.
        let wait = unsafe { CreateThreadpoolWait(Some(posted), port, std::ptr::null()) };
        if wait == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SocketWait { wait })
    }

    /// Waits once for `event` to be signalled, which posts the packet.
    fn set(&self, event: windows_sys::Win32::Foundation::HANDLE) {
        // SAFETY: our wait, on an event its poller keeps open while it is set; no timeout.
        unsafe {
            windows_sys::Win32::System::Threading::SetThreadpoolWait(self.wait, event, std::ptr::null())
        };
    }
}

#[cfg(windows)]
impl Drop for SocketWait {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::{
            CloseThreadpoolWait, SetThreadpoolWait, WaitForThreadpoolWaitCallbacks,
        };
        // SAFETY: our wait: unset, its callbacks waited out, those not begun cancelled, and
        // closed, before its port can go.
        unsafe {
            SetThreadpoolWait(self.wait, std::ptr::null_mut(), std::ptr::null());
            WaitForThreadpoolWaitCallbacks(self.wait, 1);
            CloseThreadpoolWait(self.wait);
        }
    }
}

#[cfg(windows)]
impl Poller {
    pub fn new() -> io::Result<Poller> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Networking::WinSock::{WSA_INVALID_EVENT, WSACreateEvent};
        crate::platform::winsock()?;
        let own = completion_port()?;
        // SAFETY: no arguments.
        let event = unsafe { WSACreateEvent() };
        if event == WSA_INVALID_EVENT {
            return Err(io::Error::last_os_error());
        }
        Ok(Poller {
            sockets: None,
            port: own.as_raw_handle(),
            own: Some(own),
            event,
            fds: Vec::new(),
            entries: Vec::new(),
            keyed: std::collections::HashMap::new(),
        })
    }

    /// Takes its packets from `port` from here on: the device's waker's, to which it posts
    /// its wakes and its connections' pipes tell their calls' ends (D272); its owner keeps it
    /// open while the poller waits. Given before the first wait, as the device's worker gives
    /// it, so that nothing it waited on was told of on its own port.
    pub fn watch(&mut self, port: windows_sys::Win32::Foundation::HANDLE) {
        self.sockets = None;
        self.port = port;
        self.own = None;
    }

    /// The port its waits take packets from, which a test's pipes are attached to as the
    /// device's are to its waker's.
    #[cfg(test)]
    pub fn port(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.port
    }

    /// Waits until an interest is ready or `timeout` passes; appends what is ready to
    /// `out`.
    ///
    /// Every interest's socket is associated with the event anew each wait, as a socket
    /// closed and its handle reused since the last is a socket the event knows nothing of.
    /// The event is reset before the sockets are looked at, so that anything after the look
    /// sets it, which the thread-pool wait set before the port's wait posts. Pipes ready as
    /// their owner looked are ready now, and the rest are named by their packets: a pipes'
    /// call that ended after its owner's look posted one, as its handles were associated
    /// before it.
    pub fn wait<T: Copy>(
        &mut self,
        interests: &[Interest<T>],
        timeout: Option<Duration>,
        out: &mut Vec<Ready<T>>,
    ) -> io::Result<()> {
        use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
        use windows_sys::Win32::Networking::WinSock::{
            FD_ACCEPT, FD_CLOSE, FD_READ, FD_WRITE, WSAEventSelect, WSAResetEvent,
        };
        use windows_sys::Win32::System::IO::{GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY};
        use windows_sys::Win32::System::Threading::INFINITE;
        let mut sockets = false;
        for i in interests {
            if let Source::Socket(fd) = i.fd {
                sockets = true;
                // SAFETY: a socket of the caller's and our own event. One that fails is
                // reported by the look below (POLLNVAL).
                unsafe { WSAEventSelect(fd, self.event, (FD_READ | FD_WRITE | FD_CLOSE | FD_ACCEPT) as i32) };
            }
        }
        // SAFETY: our own event.
        unsafe { WSAResetEvent(self.event) };
        let before = out.len();
        self.look(interests, out)?;
        self.keyed.clear();
        for (index, i) in interests.iter().enumerate() {
            if let Source::Pipes { key, ready } = i.fd {
                match key {
                    Some(key) if !ready => {
                        self.keyed.insert(key, index);
                    }
                    _ => out.push(Ready {
                        token: i.token,
                        read: i.read,
                        write: i.write,
                    }),
                }
            }
        }
        let now = out.len() > before || timeout == Some(Duration::ZERO);
        if !now && sockets {
            if self.sockets.is_none() {
                self.sockets = Some(SocketWait::new(self.port)?);
            }
            if let Some(wait) = &self.sockets {
                wait.set(self.event as windows_sys::Win32::Foundation::HANDLE);
            }
        }
        // Rounded up, so a deadline is never missed by waking a moment early; none waits
        // without end (INFINITE). Ready now, it takes what packets there are and waits for
        // none, so that those told of already are not told of again next wait.
        let ms = match timeout {
            _ if now => 0,
            None => INFINITE,
            Some(t) => u32::try_from(t.as_micros().div_ceil(1000))
                .unwrap_or(INFINITE - 1)
                .min(INFINITE - 1),
        };
        // Room for a packet of each interest's key, the waker's and the sockets': what one
        // wait can learn of; more are the next wait's.
        let room = interests.len().saturating_add(2);
        self.entries.clear();
        self.entries.resize(room, OVERLAPPED_ENTRY::default());
        let mut got = 0u32;
        // SAFETY: the port, and room for `room` entries; not alertable.
        let took = unsafe {
            GetQueuedCompletionStatusEx(
                self.port,
                self.entries.as_mut_ptr(),
                u32::try_from(room).unwrap_or(u32::MAX),
                &mut got,
                ms,
                0,
            )
        };
        if took == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != i32::try_from(WAIT_TIMEOUT).ok() {
                return Err(e);
            }
            got = 0;
        }
        for entry in self.entries.iter().take(usize::try_from(got).unwrap_or(0)) {
            // A packet of a pair gone, or told of already, names none: its key is no
            // interest's, never reused.
            if let Some(index) = self.keyed.remove(&entry.lpCompletionKey)
                && let Some(i) = interests.get(index)
            {
                out.push(Ready {
                    token: i.token,
                    read: i.read,
                    write: i.write,
                });
            }
        }
        if !now && sockets {
            self.look(interests, out)?;
        }
        Ok(())
    }

    /// What is ready of `interests`' sockets now: a WSAPoll that does not wait. End-of-stream
    /// is a hangup (POLLHUP) here, and a read's alone, never a reason to wake a writer.
    fn look<T: Copy>(&mut self, interests: &[Interest<T>], out: &mut Vec<Ready<T>>) -> io::Result<()> {
        use windows_sys::Win32::Networking::WinSock::{
            POLLERR, POLLHUP, POLLNVAL, POLLRDNORM, POLLWRNORM, WSAPOLLFD, WSAPoll,
        };
        let polled = || interests.iter().filter(|i| matches!(i.fd, Source::Socket(_)));
        self.fds.clear();
        self.fds.extend(polled().filter_map(|i| match i.fd {
            Source::Socket(fd) => Some(WSAPOLLFD {
                fd,
                events: (if i.read { POLLRDNORM } else { 0 }) | (if i.write { POLLWRNORM } else { 0 }),
                revents: 0,
            }),
            Source::Pipes { .. } => None,
        }));
        if self.fds.is_empty() {
            return Ok(());
        }
        // SAFETY: `fds` is a valid array of `fds.len()` entries; no wait.
        let n = unsafe { WSAPoll(self.fds.as_mut_ptr(), self.fds.len() as u32, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        for (f, i) in self.fds.iter().zip(polled()) {
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
        // Its sockets' wait first, which waits on the event and posts to the port.
        self.sockets = None;
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

    /// A wait wakes for data that comes while it waits, as the vsock device's waits do for
    /// a host's stream: its handshake line read first, then a wait in which another socket
    /// alone is asked about, then the reply, and the stream's data only after the reply.
    #[test]
    fn a_wait_wakes_for_data_that_comes_while_it_waits() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let fd = socket(ours.raw_socket());
        let mut poller = Poller::new().unwrap();
        let mut out = Vec::new();
        let read = |token| Interest {
            fd,
            read: true,
            write: false,
            token,
        };
        (&theirs).write_all(b"CONNECT 1\n").unwrap();
        poller
            .wait(&[read(1)], Some(Duration::from_secs(10)), &mut out)
            .unwrap();
        assert!(out.iter().any(|r| r.token == 1 && r.read), "{out:?}");
        let mut line = [0u8; 10];
        std::io::Read::read_exact(&mut &ours, &mut line).unwrap();
        let (other, _peer) = UnixStream::pair().unwrap();
        out.clear();
        poller
            .wait(
                &[Interest {
                    fd: socket(other.raw_socket()),
                    read: true,
                    write: false,
                    token: 2,
                }],
                Some(Duration::from_millis(20)),
                &mut out,
            )
            .unwrap();
        assert!(out.is_empty(), "{out:?}");
        (&ours).write_all(b"OK 1\n").unwrap();
        let client = std::thread::spawn(move || {
            let mut reply = [0u8; 5];
            std::io::Read::read_exact(&mut &theirs, &mut reply).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            (&theirs).write_all(b"data").unwrap();
            theirs
        });
        out.clear();
        let waited = std::time::Instant::now();
        poller
            .wait(&[read(3)], Some(Duration::from_secs(10)), &mut out)
            .unwrap();
        let took = waited.elapsed();
        assert!(
            out.iter().any(|r| r.token == 3 && r.read),
            "no data after {took:?}: {out:?}"
        );
        assert!(
            took < Duration::from_secs(5),
            "the data woke the wait after {took:?}"
        );
        drop(client.join().unwrap());
    }

    /// On Windows a wait takes the port's packets (D272): a pipes' interest its owner found
    /// ready is ready at once; one that was not is named by its key's packet, once; a packet
    /// of a key no interest has, of a pair gone, names none; and a wake posted to the port
    /// ends a wait with nothing ready.
    #[cfg(windows)]
    #[test]
    fn a_wait_takes_its_ports_packets_by_their_keys() {
        use windows_sys::Win32::System::IO::PostQueuedCompletionStatus;
        let mut poller = Poller::new().unwrap();
        let pipes = |key, ready| Interest {
            fd: Source::Pipes {
                key: Some(key),
                ready,
            },
            read: true,
            write: false,
            token: key as u8,
        };
        let mut out = Vec::new();
        poller
            .wait(&[pipes(5, true)], Some(Duration::from_secs(10)), &mut out)
            .unwrap();
        assert!(out.iter().any(|r| r.token == 5), "{out:?}");
        let port = poller.port();
        let post = |key| {
            // SAFETY: the poller's port, open while it lives, and no OVERLAPPED.
            let posted = unsafe { PostQueuedCompletionStatus(port, 0, key, std::ptr::null()) };
            assert_ne!(posted, 0, "{}", io::Error::last_os_error());
        };
        post(9);
        post(6);
        post(6);
        out.clear();
        poller
            .wait(&[pipes(6, false)], Some(Duration::from_secs(10)), &mut out)
            .unwrap();
        assert_eq!(out.iter().filter(|r| r.token == 6).count(), 1, "{out:?}");
        assert!(out.iter().all(|r| r.token == 6), "a gone key named: {out:?}");
        post(WAKER);
        out.clear();
        let waited = std::time::Instant::now();
        poller
            .wait(&[pipes(7, false)], Some(Duration::from_secs(10)), &mut out)
            .unwrap();
        assert!(out.is_empty(), "{out:?}");
        assert!(waited.elapsed() < Duration::from_secs(5), "a wake slept through");
    }

    #[test]
    fn a_half_closed_peer_is_readable_but_not_a_reason_to_wake_a_writer() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        // Fill our send buffer so the socket is not writable.
        let chunk = [0u8; 4096];
        while (&ours).write(&chunk).is_ok() {}
        theirs.shutdown(Shutdown::Write).unwrap();
        let fd = socket(ours.raw_socket());
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
