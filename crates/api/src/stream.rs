//! The [`Stream`] every guest connection is: a Unix stream socket, or on Windows shards' own
//! AF_UNIX socket, non-blocking, whose reads and writes wait in a poll (poll(2), WSAPoll) to
//! their timeouts.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::Stream;

/// A connection: [`connect`]ed to a socket, or one end of a [`pair`].
#[derive(Debug)]
pub(crate) struct Socket {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    inner: shards_afunix::UnixStream,
    /// How long a read and a write wait, in ns, 0 for ever: shared by its clones, as a
    /// socket's own timeouts are.
    shared: triomphe::Arc<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    waits: [std::sync::atomic::AtomicU64; 2],
    nonblocking: std::sync::atomic::AtomicBool,
}

impl Socket {
    /// One operation on the already nonblocking OS socket, including Interrupted.
    /// Readiness-only handshakes must not enter the normal wait/retry path.
    pub(crate) fn read_nowait(&self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.inner).read(buf)
    }

    pub(crate) fn write_nowait(&self, buf: &[u8]) -> io::Result<usize> {
        (&self.inner).write(buf)
    }

    fn shared() -> io::Result<triomphe::Arc<Shared>> {
        triomphe::Arc::try_new(Shared::default()).map_err(|_| io::ErrorKind::OutOfMemory.into())
    }

    /// Fixed, nonzero Socket storage, using Box's documented Global layout contract.
    fn allocation() -> io::Result<std::ptr::NonNull<Socket>> {
        // SAFETY: Socket is Sized and nonzero (it contains an owned OS socket and pointer).
        let at = unsafe { std::alloc::alloc(std::alloc::Layout::new::<Socket>()) };
        std::ptr::NonNull::new(at.cast()).ok_or_else(|| io::ErrorKind::OutOfMemory.into())
    }

    pub(crate) fn boxed(self) -> io::Result<Box<dyn Stream>> {
        let at = Self::allocation()?;
        // SAFETY: fresh Global storage of Socket's exact layout, initialized once; Box
        // receives its sole ownership and will drop/deallocate it with that same layout.
        unsafe {
            at.as_ptr().write(self);
            Ok(Box::from_raw(at.as_ptr()))
        }
    }
}

/// Connects to the Unix socket at `path`, waiting on its listener no longer than `wait`
/// (`None`: as long as the OS's connect takes). On Unix a connect that does not wait
/// connects or fails at once, never under way ("UNIX domain sockets failed with EAGAIN
/// instead", connect(2)): EAGAIN where the listener's queue is full (Linux) and
/// ECONNREFUSED where none listens or (macOS) its queue is full, so `wait` has nothing to
/// bound there, as a blocking one waits for room for as long as its send timeout, for ever
/// as std leaves it (unix(7)). On Windows its connect is under way until afunix.sys ends it,
/// waited out within `wait` (`shards_afunix::UnixStream::connect_within`).
pub(crate) fn connect_until(path: &Path, deadline: Option<Instant>) -> io::Result<Socket> {
    #[cfg(unix)]
    let inner = {
        let _ = deadline;
        unix::connect(path)?
    };
    #[cfg(windows)]
    let inner = shards_afunix::UnixStream::connect_until(path, deadline)?;
    Socket::new(inner)
}

/// What a [`Socket::read_watched`] or [`Socket::write_watched`] waits on beside its socket:
/// a process held for its end (`taken::Held::exit`: Linux's pidfd, macOS's kqueue with its
/// exit registered; on Windows the process's handle), which is readable, or signalled, once
/// the process has exited.
#[cfg(unix)]
pub(crate) type Exit<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub(crate) type Exit<'a> = std::os::windows::io::BorrowedHandle<'a>;

impl Socket {
    /// A read with no clock that ends at what comes, at the socket's end, or at the end of
    /// the process `exit` holds: `None` then.
    pub(crate) fn read_watched(&self, buf: &mut [u8], exit: Exit<'_>) -> io::Result<Option<usize>> {
        self.watched(waits::READ, exit, || self.read_nowait(buf))
    }

    /// A write with no clock that ends where the socket takes bytes, at its end, or at the
    /// end of the process `exit` holds: `None` then.
    pub(crate) fn write_watched(&self, buf: &[u8], exit: Exit<'_>) -> io::Result<Option<usize>> {
        self.watched(waits::WRITE, exit, || self.write_nowait(buf))
    }

    /// Does `op` as far as it goes now and, where it would block, waits for the socket and
    /// for `exit` together, with no clock: `None` once `exit`'s process has ended.
    fn watched<T>(
        &self,
        which: usize,
        exit: Exit<'_>,
        mut op: impl FnMut() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        loop {
            match op() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                done => return done.map(Some),
            }
            if self.ready_or_exited(which, exit)? {
                return Ok(None);
            }
        }
    }
}

pub(crate) fn connect_nowait(path: &Path) -> io::Result<Socket> {
    #[cfg(unix)]
    let inner = unix::connect(path)?;
    #[cfg(windows)]
    let inner = shards_afunix::UnixStream::connect_ready(path)?;
    Socket::new(inner)
}

/// Two connected ends.
pub(crate) fn pair() -> io::Result<(Socket, Socket)> {
    #[cfg(unix)]
    let (a, b) = std::os::unix::net::UnixStream::pair()?;
    #[cfg(windows)]
    let (a, b) = shards_afunix::UnixStream::pair()?;
    Ok((Socket::new(a)?, Socket::new(b)?))
}

/// Every stream's waits, on every OS: a non-blocking socket, and each read and write looping
/// its partial I/O and a poll to its own timeout.
mod waits {
    use std::io;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::Socket;

    pub(super) const READ: usize = 0;
    pub(super) const WRITE: usize = 1;

    impl Socket {
        pub(super) fn set_wait(&self, which: usize, timeout: Option<Duration>) -> io::Result<()> {
            let ns = nanos(timeout)?;
            if let Some(wait) = self.shared.waits.get(which) {
                wait.store(ns, Ordering::Relaxed);
            }
            Ok(())
        }

        /// Does `op`, a read (`READ`) or a write (`WRITE`) on the non-blocking socket, as
        /// far as it goes now, and where it would block, waits in a poll until it can go on
        /// or has ended, at most its timeout from now: then fails `WouldBlock`, as std's
        /// sockets do at theirs on Unix. A write moves what the socket takes, so a peer that
        /// stops reading holds no write past its timeout, and a read a clone raced to the
        /// bytes waits again rather than blocking.
        pub(super) fn waiting<T>(
            &self,
            which: usize,
            mut op: impl FnMut() -> io::Result<T>,
        ) -> io::Result<T> {
            if self.shared.nonblocking.load(Ordering::Relaxed) {
                return loop {
                    match op() {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        done => break done,
                    }
                };
            }
            let ns = self
                .shared
                .waits
                .get(which)
                .map_or(0, |w| w.load(Ordering::Relaxed));
            let deadline = if ns == 0 {
                None
            } else {
                Some(
                    Instant::now()
                        .checked_add(Duration::from_nanos(ns))
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
                )
            };
            loop {
                match op() {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        if deadline.is_some_and(|at| Instant::now() >= at) {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                        continue;
                    }
                    done => return done,
                }
                let wait = match deadline {
                    None => -1,
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                        // In the poll's unit, ms (poll(2): "timeout specifies the number of
                        // milliseconds"; WSAPoll's likewise), rounded up: a wait of 0 returns
                        // at once, and would spin until the deadline.
                        i32::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
                    }
                };
                self.poll_ready(which, wait)?;
            }
        }
    }

    /// A timeout in ns, 0 for none: a `Duration`'s own unit, so any timeout is kept as
    /// given (a nonzero one is at least 1 ns), and one past u64's 584 years is for ever's
    /// equal. A zero timeout is refused, as std refuses it (`UnixStream::set_read_timeout`).
    pub(super) fn nanos(timeout: Option<Duration>) -> io::Result<u64> {
        match timeout {
            None => Ok(0),
            Some(t) if t.is_zero() => Err(io::ErrorKind::InvalidInput.into()),
            Some(t) => Ok(u64::try_from(t.as_nanos()).unwrap_or(u64::MAX)),
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd as _, OwnedFd};
    use std::path::Path;

    use super::Socket;
    use super::waits::READ;

    /// `path` as a Unix socket's address, and its length: its bytes and a NUL in
    /// `sun_path` (unix(7)), refused where they don't fit or hold a NUL of their own.
    fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
        use std::os::unix::ffi::OsStrExt as _;
        // SAFETY: a sockaddr_un, plain data, zeroed.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        addr.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX).map_err(io::Error::other)?;
        for (to, from) in addr.sun_path.iter_mut().zip(bytes) {
            *to = libc::c_char::from_ne_bytes([*from]);
        }
        let len = std::mem::offset_of!(libc::sockaddr_un, sun_path)
            .saturating_add(bytes.len())
            .saturating_add(1);
        #[cfg(target_os = "macos")]
        {
            addr.sun_len = u8::try_from(len).map_err(io::Error::other)?;
        }
        Ok((addr, libc::socklen_t::try_from(len).map_err(io::Error::other)?))
    }

    /// A connect to the Unix socket at `path` that never waits on its listener: its socket
    /// made non-blocking and close-on-exec before its connect(2).
    pub(super) fn connect(path: &Path) -> io::Result<std::os::unix::net::UnixStream> {
        let (addr, len) = address(path)?;
        #[cfg(target_os = "linux")]
        let ty = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let ty = libc::SOCK_STREAM;
        // SAFETY: socket(2), plain arguments.
        let fd = unsafe { libc::socket(libc::AF_UNIX, ty, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor socket(2) just made, owned from here alone.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // macOS's socket(2) takes neither flag: set as std sets them on a socket it makes.
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: fcntl(2) of the descriptor `fd` owns, setting its close-on-exec flag.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fcntl(2) of the descriptor `fd` owns, making it non-blocking.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: connect(2) of the socket `fd` owns to an address of ours, `len` its length.
        if unsafe { libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::os::unix::net::UnixStream::from(fd))
    }

    impl Socket {
        /// Its socket made non-blocking, its waits this value's (`waits`); and on macOS a
        /// write to a peer that has gone made to fail EPIPE, never to raise SIGPIPE, which
        /// ends a process that leaves it at its default: SO_NOSIGPIPE, which std sets on a
        /// socket it connects but not on a pair. Linux's std sends with MSG_NOSIGNAL.
        pub(super) fn new(inner: std::os::unix::net::UnixStream) -> io::Result<Socket> {
            inner.set_nonblocking(true)?;
            #[cfg(target_vendor = "apple")]
            no_sigpipe(&inner)?;
            Ok(Socket {
                inner,
                shared: Socket::shared()?,
            })
        }

        /// Waits in poll(2) at most `ms` (-1: for ever) for it to be readable (`READ`) or
        /// writable, or to have ended.
        pub(super) fn poll_ready(&self, which: usize, ms: i32) -> io::Result<()> {
            let mut fd = libc::pollfd {
                fd: self.inner.as_raw_fd(),
                events: if which == READ {
                    libc::POLLIN
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            };
            // SAFETY: poll(2) of one pollfd, of a socket this value holds open.
            let n = unsafe { libc::poll(&raw mut fd, 1, ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
            Ok(())
        }
    }

    impl Socket {
        /// Waits in poll(2), with no clock, for it to be readable (`READ`) or writable, or to
        /// have ended, and for `exit` to be readable, as a pidfd (pidfd_open(2)) and a kqueue
        /// holding an event (kqueue(2)) are once their process has exited: whether `exit` is.
        /// An interrupted poll is neither, and its caller asks again.
        pub(super) fn ready_or_exited(&self, which: usize, exit: super::Exit<'_>) -> io::Result<bool> {
            let mut fds = [
                libc::pollfd {
                    fd: self.inner.as_raw_fd(),
                    events: if which == READ {
                        libc::POLLIN
                    } else {
                        libc::POLLOUT
                    },
                    revents: 0,
                },
                libc::pollfd {
                    fd: exit.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll(2) of two pollfds of ours, of a socket this value holds open and a
            // descriptor `exit` borrows, with no timeout.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
                return Ok(false);
            }
            let [_, exited] = fds;
            Ok(exited.revents != 0)
        }
    }

    /// Sets SO_NOSIGPIPE on `s` (setsockopt(2); the macOS SDK's `sys/socket.h`: "No SIGPIPE
    /// on EPIPE"), so that a write to a gone peer fails EPIPE.
    #[cfg(target_vendor = "apple")]
    fn no_sigpipe(s: &std::os::unix::net::UnixStream) -> io::Result<()> {
        let on: libc::c_int = 1;
        let size = std::mem::size_of::<libc::c_int>();
        let len = libc::socklen_t::try_from(size).map_err(io::Error::other)?;
        // SAFETY: setsockopt(2) of a socket `s` holds open, from a c_int of ours, its length
        // given in `len`.
        let r = unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const on).cast(),
                len,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// A buffer size getsockopt(2) reported, `what` naming it: zero or less is an error,
    /// not a size.
    pub(super) fn size(what: &str, reported: libc::c_int) -> io::Result<usize> {
        usize::try_from(reported)
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| io::Error::other(format!("{what} {reported}: not a buffer size")))
    }

    impl Socket {
        /// A SOL_SOCKET buffer size option, SO_SNDBUF or SO_RCVBUF, as getsockopt(2) reads it
        /// (socket(7)).
        pub(super) fn buffer_size(&self, name: libc::c_int, what: &str) -> io::Result<usize> {
            let mut value: libc::c_int = 0;
            let size = std::mem::size_of::<libc::c_int>();
            let mut len = libc::socklen_t::try_from(size).map_err(io::Error::other)?;
            // SAFETY: getsockopt(2) of a socket this value holds open, into a c_int of ours,
            // its length given in `len`, which is checked on return to be that c_int's.
            let r = unsafe {
                libc::getsockopt(
                    self.inner.as_raw_fd(),
                    libc::SOL_SOCKET,
                    name,
                    (&raw mut value).cast(),
                    &raw mut len,
                )
            };
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            if usize::try_from(len).ok() != Some(size) {
                return Err(io::Error::other(format!("{what}: {len} bytes, not a c_int")));
            }
            self::size(what, value)
        }
    }
}

#[cfg(windows)]
impl Socket {
    /// Its socket made non-blocking, its waits this value's (`waits`), as on Unix: shards'
    /// AF_UNIX socket's own timeouts poll and then send a whole buffer, which a peer that
    /// stops reading holds for ever.
    fn new(inner: shards_afunix::UnixStream) -> io::Result<Socket> {
        inner.set_nonblocking(true)?;
        Ok(Socket {
            inner,
            shared: Socket::shared()?,
        })
    }

    /// Waits in WSAPoll at most `ms` (-1: for ever) for it to be readable (`READ`) or
    /// writable, or to have ended.
    fn poll_ready(&self, which: usize, ms: i32) -> io::Result<()> {
        self.inner.ready_within(which == waits::WRITE, ms).map(drop)
    }

    /// Waits, with no clock, for it to be readable (`READ`) or writable, or to have ended,
    /// and for the process `exit` to be signalled, as it is once it has exited: whether it
    /// is. WSAPoll takes sockets alone, so the socket's readiness is an event
    /// (WSAEventSelect, FD_READ or FD_WRITE, and FD_CLOSE: signalled at once where the
    /// condition holds as it is selected), waited on with the process
    /// (`shards_ipc::await_any`, WaitForMultipleObjects), and let go of after, the socket left
    /// non-blocking, as `new` made it.
    fn ready_or_exited(&self, which: usize, exit: Exit<'_>) -> io::Result<bool> {
        use std::os::windows::io::AsHandle as _;
        use windows_sys::Win32::Networking::WinSock::{
            FD_CLOSE, FD_READ, FD_WRITE, WSAEVENT, WSAEventSelect,
        };
        let wanted = if which == waits::WRITE { FD_WRITE } else { FD_READ };
        let wanted = i32::try_from(wanted | FD_CLOSE).map_err(io::Error::other)?;
        let event = shards_ipc::Event::new()?;
        // SAFETY: the socket this value holds, and an event of ours, which outlives the
        // selection: it is let go of below.
        if unsafe { WSAEventSelect(self.inner.raw(), event.raw() as WSAEVENT, wanted) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let waited = shards_ipc::await_any(&[], &[event.as_handle(), exit], None);
        // SAFETY: as above: no event of the socket's is set from here on.
        unsafe { WSAEventSelect(self.inner.raw(), 0, 0) };
        let (_, signalled) = waited?;
        Ok(signalled.get(1).copied().unwrap_or(false))
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Waited for in poll(2), which sees a peer's half-close that a read waiting on a
        // macOS socket can sleep through (PM M236).
        Stream::read_shared(self, buf)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Stream::write_shared(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stream for Socket {
    fn try_clone(&self) -> io::Result<Box<dyn Stream>> {
        let inner = self.inner.try_clone()?;
        let at = Self::allocation()?;
        // Each additional strong count belongs to this independently owned socket and
        // ALREADY allocated box. Forgotten boxes retain their nonoverlapping allocation.
        // On the supported 64-bit targets Socket occupies at least two pointer widths:
        // even the entire usize address space holds fewer than isize::MAX such boxes,
        // plus the one original possibly unboxed Socket. Thus Triomphe's refcount-abort
        // precondition is unreachable here. No Arc/count/raw API escapes this module.
        // SAFETY: as in boxed; no fallible operation remains after storage is acquired.
        unsafe {
            at.as_ptr().write(Socket {
                inner,
                shared: self.shared.clone(),
            });
            Ok(Box::from_raw(at.as_ptr()))
        }
    }

    fn read_shared(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.waiting(waits::READ, || self.read_nowait(buf))
    }

    fn write_shared(&self, buf: &[u8]) -> io::Result<usize> {
        self.waiting(waits::WRITE, || self.write_nowait(buf))
    }

    fn into_socket(self: Box<Self>) -> crate::OwnedSocket {
        crate::OwnedSocket::from(self.inner)
    }

    fn handle(&self) -> crate::Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.inner);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.inner)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.shared
            .nonblocking
            .store(on, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        // Both ways one way at a time: XNU shuts both at once or neither, and neither once
        // the peer has half-closed (ENOTCONN), so the peer would never read this end (D175,
        // PM M236).
        let one = |how| match self.inner.shutdown(how) {
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()),
            other => other,
        };
        match how {
            Shutdown::Both => {
                one(Shutdown::Write)?;
                one(Shutdown::Read)
            }
            how => self.inner.shutdown(how),
        }
    }

    fn buffer_sizes(&self) -> io::Result<(usize, usize)> {
        #[cfg(unix)]
        return Ok((
            self.buffer_size(libc::SO_SNDBUF, "SO_SNDBUF")?,
            self.buffer_size(libc::SO_RCVBUF, "SO_RCVBUF")?,
        ));
        #[cfg(windows)]
        Ok((self.inner.send_buffer_size()?, self.inner.recv_buffer_size()?))
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_wait(waits::READ, timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_wait(waits::WRITE, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::while_runs;
    use std::time::Instant;

    /// Bytes go both ways, a half-close reaches the peer as its end, and the other way
    /// still carries bytes after it (D12's half-close; D215).
    #[test]
    fn a_pair_carries_bytes_both_ways_and_a_half_close() {
        let (mut a, mut b) = pair().unwrap();
        a.write_all(b"ping").unwrap();
        Stream::shutdown(&a, Shutdown::Write).unwrap();
        let mut got = Vec::new();
        b.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"ping");
        b.write_all(b"pong").unwrap();
        Stream::shutdown(&b, Shutdown::Both).unwrap();
        let mut back = Vec::new();
        a.read_to_end(&mut back).unwrap();
        assert_eq!(back, b"pong");
    }

    /// A read with nothing to read fails at its timeout, and leaves the connection as it
    /// was: what comes later is read (D215).
    #[test]
    fn a_read_times_out_and_the_connection_goes_on() {
        let (mut a, mut b) = pair().unwrap();
        // An input: any timeout shows it kept.
        Stream::set_read_timeout(&a, Some(Duration::from_millis(50))).unwrap();
        let start = Instant::now();
        let e = a.read(&mut [0; 4]).unwrap_err();
        assert!(
            matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut),
            "{e:?}"
        );
        assert!(start.elapsed() >= Duration::from_millis(50));
        b.write_all(b"late").unwrap();
        let mut got = [0; 4];
        a.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"late");
        assert!(Stream::set_read_timeout(&a, Some(Duration::ZERO)).is_err());
    }

    /// A timeout set after its peer answered and closed (PM M579) is taken, as a stream's waits
    /// are its own (`waits`), no socket option that macOS refuses on an ended socket, and the
    /// answer is read whole, then its end: an API client reads a fully answered request.
    #[test]
    fn a_timeout_set_after_its_peer_answered_and_closed_reads_the_answer() {
        let (mut a, mut b) = pair().unwrap();
        b.write_all(b"answer").unwrap();
        drop(b);
        Stream::set_read_timeout(&a, Some(Duration::from_secs(5))).unwrap();
        Stream::set_write_timeout(&a, Some(Duration::from_secs(5))).unwrap();
        let mut answer = Vec::new();
        a.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, b"answer");
    }

    #[test]
    fn already_queued_data_is_read_before_waiting_out_a_timeout() {
        let (mut a, mut b) = pair().unwrap();
        b.write_all(b"x").unwrap();
        // The shortest nonzero timeout bounds waiting, not the initial nonblocking I/O.
        a.set_read_timeout(Some(Duration::from_nanos(1))).unwrap();
        let mut byte = [0; 1];
        assert_eq!(a.read(&mut byte).unwrap(), 1);
        assert_eq!(&byte, b"x");
    }

    /// A timeout is kept as given, to its nanosecond: none rounded down to a shorter one or
    /// up to poll(2)'s millisecond, and none of them for ever (D215).
    #[test]
    fn a_timeout_is_kept_as_given() {
        assert_eq!(
            waits::nanos(Some(Duration::from_micros(1500))).unwrap(),
            1_500_000
        );
        assert_eq!(waits::nanos(Some(Duration::from_nanos(1))).unwrap(), 1);
        assert_eq!(waits::nanos(None).unwrap(), 0);
        assert!(waits::nanos(Some(Duration::ZERO)).is_err());
        assert_eq!(waits::nanos(Some(Duration::MAX)).unwrap(), u64::MAX);
    }

    /// A write to a peer that stops reading fails at its timeout, having moved what the
    /// socket took, rather than blocking for ever: its writer's answer waited for as the fact
    /// it is, however late its host runs it (D215; review B3).
    #[test]
    fn a_write_to_a_peer_that_stops_reading_ends_at_its_timeout() {
        // The least wait this host's streams keep, measured: any kept timeout shows it.
        let timeout = while_runs::kept_wait();
        let (mut a, _b) = pair().unwrap();
        Stream::set_write_timeout(&a, Some(timeout)).unwrap();
        // An input: more than any socket's buffer holds (macOS's 8 KiB, Linux's wmem_max), so
        // the peer's not reading is what stops it.
        let big = vec![7u8; 64 << 20];
        let wrote = std::thread::spawn(move || a.write_all(&big).map_err(|e| e.kind()))
            .join()
            .unwrap();
        // Winsock may take the whole buffer from a non-blocking send (jupiter took 64 MiB);
        // what the write may not do is block past its timeout.
        assert!(
            matches!(
                wrote,
                Ok(()) | Err(io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
            ),
            "{wrote:?}"
        );
    }

    /// Set in the child `a_write_to_a_gone_peer_fails_epipe_where_sigpipe_kills` runs.
    #[cfg(unix)]
    const SIGPIPE_CHILD: &str = "SHARDS_API_SIGPIPE_CHILD";

    /// A write to a peer that has gone fails EPIPE, never SIGPIPE, in a process whose
    /// SIGPIPE is its default, which ends it: a library's write never ends the program that
    /// links it, whatever that program does with SIGPIPE (integrate's 80078b00 sent with
    /// MSG_NOSIGNAL). Both kinds of stream: a pair (the simulator's) and one connected to a
    /// listener (the client's). Run in a child of this test binary, as the default would end
    /// every test beside it, which says it ran each.
    #[cfg(unix)]
    #[test]
    fn a_write_to_a_gone_peer_fails_epipe_where_sigpipe_kills() {
        const NAME: &str = "stream::tests::a_write_to_a_gone_peer_fails_epipe_where_sigpipe_kills";
        const RAN: &str = "sigpipe child: EPIPE from a pair and a connection";
        if std::env::var_os(SIGPIPE_CHILD).is_some() {
            // SAFETY: signal(2) setting SIGPIPE back to its default, in this child alone,
            // which runs this test and nothing else.
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            let (mut paired, peer) = pair().unwrap();
            drop(peer);
            let dir = shards_testdir::TempDir::new("sp").unwrap();
            let at = dir.path().join("s");
            let listener = std::os::unix::net::UnixListener::bind(&at).unwrap();
            let mut connected = connect_until(&at, None).unwrap();
            drop(listener.accept().unwrap());
            for (what, s) in [("a pair", &mut paired), ("a connection", &mut connected)] {
                let e = s.write(b"x").unwrap_err();
                assert_eq!(e.raw_os_error(), Some(libc::EPIPE), "{what}: {e:?}");
            }
            println!("{RAN}");
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
            .env(SIGPIPE_CHILD, "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&child.stdout);
        assert!(
            child.status.success() && said.contains(RAN),
            "{:?}\n{said}{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
    }

    /// Two clones reading one byte: one reads it, the other fails at its timeout rather
    /// than blocking after a poll said the byte was there; each reader's answer waited for as
    /// the fact it is (D215; review B3).
    #[test]
    fn a_read_a_clone_raced_ends_at_its_timeout() {
        // The least wait this host's streams keep, measured: any kept timeout shows it.
        let timeout = while_runs::kept_wait();
        let (a, mut b) = pair().unwrap();
        Stream::set_read_timeout(&a, Some(timeout)).unwrap();
        // The byte first: each reader's poll finds it there, and one of them takes it.
        b.write_all(b"x").unwrap();
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let mut reader = Stream::try_clone(&a).unwrap();
                std::thread::spawn(move || reader.read(&mut [0u8; 1]).map_err(|e| e.kind()))
            })
            .collect();
        let mut got: Vec<_> = readers.into_iter().map(|r| r.join().unwrap()).collect();
        got.sort_by_key(|r| r.is_err());
        assert_eq!(got[0], Ok(1));
        assert!(
            matches!(got[1], Err(io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "{got:?}"
        );
    }

    /// A connected socket's buffer sizes are the OS's, non-zero, both ways (D215; review A).
    #[test]
    fn a_connection_reports_its_buffer_sizes() {
        let (a, b) = pair().unwrap();
        for end in [&a, &b] {
            let (send, receive) = Stream::buffer_sizes(end).unwrap();
            println!("buffer_sizes: send {send}, receive {receive}");
            assert!(send > 0 && receive > 0, "{send} {receive}");
        }
    }

    /// A zero or negative buffer size the OS reports is an error, not a size (review A).
    #[cfg(unix)]
    #[test]
    fn a_buffer_size_of_zero_or_less_is_an_error() {
        assert_eq!(unix::size("SO_SNDBUF", 8192).unwrap(), 8192);
        assert!(unix::size("SO_SNDBUF", 0).is_err());
        assert!(unix::size("SO_RCVBUF", -1).is_err());
    }

    /// Non-blocking, a read with nothing to read fails `WouldBlock` at once even with no
    /// timeout set (it would otherwise wait for ever), and so does its clone's; once the peer
    /// writes, the handle polls readable and the read takes the bytes (Hex's event loop,
    /// native plan step 8).
    #[test]
    fn a_nonblocking_read_fails_at_once_and_its_handle_says_when_to_read() {
        // Neither the read nor the poll below waits for anything but its fact: each is looked
        // at while it runs, each millisecond (poll(2)'s unit), and fails the test if blocked.
        let at_once = Duration::from_millis(1);
        let (a, mut b) = pair().unwrap();
        Stream::set_nonblocking(&a, true).unwrap();
        let mut clone = Stream::try_clone(&a).unwrap();
        let (worker, rx) = while_runs::on_a_thread(move || {
            let mut buf = [0u8; 8];
            (clone.read(&mut buf).map_err(|e| e.kind()), clone)
        });
        let (empty, mut clone) = while_runs::answer(&rx, worker, at_once);
        assert_eq!(empty, Err(io::ErrorKind::WouldBlock));
        b.write_all(b"now").unwrap();
        #[cfg(unix)]
        {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&Stream::handle(&a));
            let (worker, rx) = while_runs::on_a_thread(move || {
                let mut fd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll(2) of one pollfd, of a socket `a` holds open until it answers.
                let n = unsafe { libc::poll(&raw mut fd, 1, -1) };
                (n, fd.revents & libc::POLLIN)
            });
            assert_eq!(while_runs::answer(&rx, worker, at_once), (1, libc::POLLIN));
        }
        let mut buf = [0u8; 8];
        let n = clone.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"now");
    }

    /// Do: share one stream between two threads as `&dyn Stream`, no clone made: one reads
    /// what the peer writes while the other writes to it. Expect each way's bytes whole, and
    /// on Windows the stream's socket, and a clone's, not inheritable, as std's Command would
    /// hand an inheritable one to every child another thread starts (Hex's audit D-5, PM M582).
    #[test]
    fn two_threads_share_a_stream_through_a_reference() {
        let (a, b) = pair().unwrap();
        let (a, b): (Box<dyn Stream>, Box<dyn Stream>) = (a.boxed().unwrap(), b.boxed().unwrap());
        let shared: &dyn Stream = &*a;
        let (sent, got) = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut got = [0u8; 4];
                (&*shared).read_exact(&mut got).unwrap();
                got
            });
            let writer = scope.spawn(|| (&*shared).write_all(b"from").unwrap());
            (&*b).write_all(b"to a").unwrap();
            writer.join().unwrap();
            let mut sent = [0u8; 4];
            (&*b).read_exact(&mut sent).unwrap();
            (sent, reader.join().unwrap())
        });
        assert_eq!((&sent, &got), (b"from", b"to a"));
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket as _;
            let inheritable = |s: crate::Handle<'_>| {
                let mut flags = 0u32;
                // SAFETY: a socket the stream holds open, and a u32 for the call to fill.
                let ok = unsafe {
                    windows_sys::Win32::Foundation::GetHandleInformation(
                        s.as_raw_socket() as windows_sys::Win32::Foundation::HANDLE,
                        &raw mut flags,
                    )
                };
                assert_ne!(ok, 0, "{}", io::Error::last_os_error());
                flags & windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT != 0
            };
            let clone = Stream::try_clone(&*a).unwrap();
            assert!(!inheritable(a.handle()), "the stream's socket");
            assert!(!inheritable(clone.handle()), "its clone's");
        }
    }

    /// Do: take a stream's socket. Expect the same connection, owned by the caller: bytes
    /// written to it reach the peer, and the peer's reach it.
    #[test]
    fn a_streams_socket_is_handed_over() {
        let (a, mut b) = pair().unwrap();
        let socket = a.boxed().unwrap().into_socket();
        #[cfg(unix)]
        let taken = std::os::unix::net::UnixStream::from(socket);
        #[cfg(windows)]
        let taken = shards_afunix::UnixStream::from(socket);
        taken.set_nonblocking(false).unwrap();
        (&taken).write_all(b"taken").unwrap();
        let mut got = [0u8; 5];
        b.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"taken");
        b.write_all(b"back").unwrap();
        let mut back = [0u8; 4];
        (&taken).read_exact(&mut back).unwrap();
        assert_eq!(&back, b"back");
    }

    /// A clone is the same connection: what one writes the peer reads (D215).
    #[test]
    fn a_clone_is_the_same_connection() {
        let (a, mut b) = pair().unwrap();
        let mut c = Stream::try_clone(&a).unwrap();
        c.write_all(b"via clone").unwrap();
        drop(c);
        Stream::shutdown(&a, Shutdown::Write).unwrap();
        let mut got = Vec::new();
        b.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"via clone");
    }
}
