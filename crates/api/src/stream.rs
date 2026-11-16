//! The [`Stream`] every guest connection is: a Unix stream socket, non-blocking, whose reads
//! and writes wait in a poll (poll(2)) to their timeouts; on Windows its two pipes (D272),
//! its way down read and its way up written, each call overlapped and waited for to its
//! timeout ([`pipes`]).

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
    /// Its pipes, which its clones share, as a socket's descriptors share it.
    #[cfg(windows)]
    inner: triomphe::Arc<pipes::Pipes>,
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
        #[cfg(unix)]
        return (&self.inner).read(buf);
        #[cfg(windows)]
        self.inner.read_nowait(buf)
    }

    pub(crate) fn write_nowait(&self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return (&self.inner).write(buf);
        #[cfg(windows)]
        self.inner.write_nowait(buf)
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
/// as std leaves it (unix(7)). On Windows the pair of pipes its file at `path` names, opened
/// its way down first, each waited for while another client holds it, to `deadline`
/// (`shards_ipc::vsock_open`, D272).
pub(crate) fn connect_until(path: &Path, deadline: Option<Instant>) -> io::Result<Socket> {
    #[cfg(unix)]
    {
        let _ = deadline;
        Socket::new(unix::connect(path)?)
    }
    #[cfg(windows)]
    {
        let (down, up) = shards_ipc::vsock_open(path, deadline)?;
        Socket::of_pipes(down, up)
    }
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
    return Socket::new(unix::connect(path)?);
    // A pair another client holds waits for nothing here: `WouldBlock`, as a socket's full
    // backlog is.
    #[cfg(windows)]
    match shards_ipc::vsock_open(path, Some(Instant::now())) {
        Ok((down, up)) => Socket::of_pipes(down, up),
        Err(e) if e.kind() == io::ErrorKind::TimedOut => Err(io::ErrorKind::WouldBlock.into()),
        Err(e) => Err(e),
    }
}

/// Two connected ends: on Windows a pipe each way, as a guest connection's are (D272), each
/// way's quota a guest connection's (`shards_ipc::VSOCK_QUOTA`).
pub(crate) fn pair() -> io::Result<(Socket, Socket)> {
    #[cfg(unix)]
    {
        let (a, b) = std::os::unix::net::UnixStream::pair()?;
        Ok((Socket::new(a)?, Socket::new(b)?))
    }
    #[cfg(windows)]
    {
        let quota = usize::try_from(shards_ipc::VSOCK_QUOTA).map_err(io::Error::other)?;
        let window = shards_ipc::Window::frame(quota)?;
        let (a_to_b, b_from_a) = shards_ipc::byte_pair(window)?;
        let (b_to_a, a_from_b) = shards_ipc::byte_pair(window)?;
        Ok((
            Socket::of_pipes(a_from_b, a_to_b)?,
            Socket::of_pipes(b_from_a, b_to_a)?,
        ))
    }
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
    /// The connection whose way down is `down` and way up `up`, ends of their pipes opened for
    /// overlapped calls; its waits this value's (`waits`), as on Unix.
    fn of_pipes(
        down: std::os::windows::io::OwnedHandle,
        up: std::os::windows::io::OwnedHandle,
    ) -> io::Result<Socket> {
        let inner = triomphe::Arc::try_new(pipes::Pipes::new(down, up)?)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        Ok(Socket {
            inner,
            shared: Socket::shared()?,
        })
    }

    /// Waits at most `ms` (-1: for ever) for it to be readable (`READ`) or writable, or to
    /// have ended.
    fn poll_ready(&self, which: usize, ms: i32) -> io::Result<()> {
        let deadline = u64::try_from(ms)
            .ok()
            .and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms)));
        if which == waits::WRITE {
            self.inner.writable_by(deadline)
        } else {
            self.inner.readable_by(deadline)
        }
    }

    /// Waits, with no clock, for it to be readable (`READ`) or writable, or to have ended,
    /// and for the process `exit` to be signalled, as it is once it has exited: whether it
    /// is.
    fn ready_or_exited(&self, which: usize, exit: Exit<'_>) -> io::Result<bool> {
        if which == waits::WRITE {
            self.inner.writable_or_exited(exit)
        } else {
            self.inner.readable_or_exited(exit)
        }
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
        #[cfg(unix)]
        let inner = self.inner.try_clone()?;
        #[cfg(windows)]
        let inner = self.inner.clone();
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
        #[cfg(unix)]
        return crate::OwnedSocket::from(self.inner);
        #[cfg(windows)]
        self.inner.owned()
    }

    fn handle(&self) -> crate::Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.inner);
        #[cfg(windows)]
        self.inner.down()
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.shared
            .nonblocking
            .store(on, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    #[cfg(windows)]
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        if matches!(how, Shutdown::Write | Shutdown::Both) {
            self.inner.shutdown_write()?;
        }
        if matches!(how, Shutdown::Read | Shutdown::Both) {
            self.inner.shutdown_read();
        }
        Ok(())
    }

    #[cfg(unix)]
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
        self.inner.buffer_sizes()
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_wait(waits::READ, timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_wait(waits::WRITE, timeout)
    }
}

/// A guest connection's two pipes on Windows (D272): its way down, read here, and its way
/// up, written here, each an end opened for overlapped calls. Reads are calls of their own,
/// each waited for to its deadline (`shards_ipc::Pipe`). A write is one call at a time, its
/// bytes this connection's until it finishes, as a socket's send buffer holds what a send
/// took: a pipe says nothing of its room (PM M549), and a cancelled write says nothing of
/// what its reader took (M603), so none is cancelled while the connection lives.
#[cfg(windows)]
mod pipes {
    use std::cell::UnsafeCell;
    use std::io;
    use std::os::windows::io::{AsHandle as _, AsRawHandle as _, BorrowedHandle, OwnedHandle};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, GetLastError};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{GetNamedPipeInfo, PIPE_SERVER_END};
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    /// Where its writes stand (R04).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Writing {
        /// None under way: the next starts at once.
        Idle,
        /// One under way, its bytes the system's until it finishes.
        UnderWay,
        /// Its writes shut down: the way up closed, which its peer reads as their end.
        Shut,
        /// A write failed with this error, which every write says from here on.
        Failed(u32),
    }

    /// Its writes: the way up, the one write's OVERLAPPED, its event, set as it finishes, and
    /// its bytes.
    struct Writes {
        up: Option<OwnedHandle>,
        /// Boxed, so that the write under way keeps its OVERLAPPED where it is.
        call: Box<UnsafeCell<OVERLAPPED>>,
        done: shards_ipc::Event,
        out: Vec<u8>,
        writing: Writing,
    }

    pub(super) struct Pipes {
        down: shards_ipc::Pipe,
        writes: Mutex<Writes>,
        /// Its reads shut down: each says the end.
        reads_ended: AtomicBool,
        /// The most one write takes: what the way up holds, so that it finishes as soon as
        /// the pipe has room for it whole.
        most: usize,
    }

    // SAFETY: the write's OVERLAPPED and bytes are the system's only while it is under way,
    // and otherwise touched only under `writes`' lock; the handles are used from any thread.
    unsafe impl Send for Pipes {}
    // SAFETY: as above.
    unsafe impl Sync for Pipes {}

    impl std::fmt::Debug for Pipes {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Pipes")
                .field("most", &self.most)
                .finish_non_exhaustive()
        }
    }

    /// `code`, a Win32 error, as an `io::Error`.
    fn error(code: u32) -> io::Error {
        io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
    }

    /// What the way `handle` carries holds, as its maker gave it (GetNamedPipeInfo): a server
    /// end reads its inbound quota and writes its outbound; a client end the other way about.
    fn quota(handle: BorrowedHandle<'_>, reads: bool) -> io::Result<usize> {
        let (mut flags, mut outbound, mut inbound) = (0u32, 0u32, 0u32);
        // SAFETY: a pipe end of ours; the flags and two quotas asked for are written to locals.
        let read = unsafe {
            GetNamedPipeInfo(
                handle.as_raw_handle(),
                &mut flags,
                &mut outbound,
                &mut inbound,
                std::ptr::null_mut(),
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        let server = flags & PIPE_SERVER_END != 0;
        let quota = if server == reads { inbound } else { outbound };
        usize::try_from(quota)
            .ok()
            .filter(|&q| q > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a pipe with no quota"))
    }

    /// A deadline in its wait's unit, ms, rounded up so that a deadline is never missed by
    /// waking a moment early; none, for ever (INFINITE).
    fn ms(deadline: Option<Instant>) -> u32 {
        deadline.map_or(INFINITE, |d| {
            u32::try_from(
                d.saturating_duration_since(Instant::now())
                    .as_micros()
                    .div_ceil(1000),
            )
            .unwrap_or(INFINITE - 1)
            .min(INFINITE - 1)
        })
    }

    /// Takes what finished of the write under way, without waiting: a part it left written
    /// on (a pipe that waits finishes a write whole, WriteFile's Remarks).
    fn settle(w: &mut Writes) {
        if w.writing != Writing::UnderWay {
            return;
        }
        let Some(up) = &w.up else {
            w.writing = Writing::Shut;
            return;
        };
        let mut moved = 0u32;
        // SAFETY: the OVERLAPPED of the write under way on `up`, not waited for.
        if unsafe { GetOverlappedResult(up.as_raw_handle(), w.call.get(), &mut moved, 0) } != 0 {
            let moved = usize::try_from(moved).unwrap_or(usize::MAX).min(w.out.len());
            w.out.drain(..moved);
            w.writing = Writing::Idle;
            if !w.out.is_empty() {
                start(w);
            }
            return;
        }
        // SAFETY: no arguments.
        match unsafe { GetLastError() } {
            ERROR_IO_INCOMPLETE => {}
            code => w.writing = Writing::Failed(code),
        }
    }

    /// Starts the write of `w`'s bytes.
    fn start(w: &mut Writes) {
        let Some(up) = &w.up else {
            w.writing = Writing::Shut;
            return;
        };
        let len = u32::try_from(w.out.len()).unwrap_or(u32::MAX);
        let ov = w.call.get();
        w.done.reset();
        // SAFETY: no write is under way: its OVERLAPPED is ours to make fresh, with our event;
        // its bytes are the system's, untouched, until it finishes (`settle`, `drop`).
        let started = unsafe {
            ov.write(OVERLAPPED {
                hEvent: w.done.raw(),
                ..OVERLAPPED::default()
            });
            WriteFile(up.as_raw_handle(), w.out.as_ptr(), len, std::ptr::null_mut(), ov)
        };
        // One that finished at once is under way still: `settle` takes it.
        let code = if started != 0 {
            ERROR_IO_PENDING
        } else {
            // SAFETY: no arguments.
            unsafe { GetLastError() }
        };
        w.writing = if code == ERROR_IO_PENDING {
            Writing::UnderWay
        } else {
            Writing::Failed(code)
        };
    }

    /// Waits for the write under way to finish, if one is.
    fn finish(w: &mut Writes) {
        while w.writing == Writing::UnderWay {
            if let Some(up) = &w.up {
                let mut moved = 0u32;
                // SAFETY: the OVERLAPPED of the write under way on `up`, waited for (bWait 1)
                // on its event; its outcome is `settle`'s to take.
                unsafe { GetOverlappedResult(up.as_raw_handle(), w.call.get(), &mut moved, 1) };
            }
            settle(w);
        }
    }

    impl Pipes {
        /// The connection whose way down is `down` and way up `up`.
        pub(super) fn new(down: OwnedHandle, up: OwnedHandle) -> io::Result<Pipes> {
            let most = quota(up.as_handle(), false)?;
            Ok(Pipes {
                down: shards_ipc::Pipe::from_handle(down),
                writes: Mutex::new(Writes {
                    up: Some(up),
                    call: Box::new(UnsafeCell::new(OVERLAPPED::default())),
                    done: shards_ipc::Event::new()?,
                    out: Vec::new(),
                    writing: Writing::Idle,
                }),
                reads_ended: AtomicBool::new(false),
                most,
            })
        }

        fn writes(&self) -> MutexGuard<'_, Writes> {
            self.writes.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// The way down's handle, which a caller's own loop may watch by a read of no bytes,
        /// finishing as something comes or the stream ends (PM M348).
        pub(super) fn down(&self) -> BorrowedHandle<'_> {
            self.down.as_handle()
        }

        /// A read of what is there now: `WouldBlock` where nothing is, its end `Ok(0)`. A
        /// clone that took the bytes first leaves this one's read nothing, which waits for
        /// nothing either.
        pub(super) fn read_nowait(&self, buf: &mut [u8]) -> io::Result<usize> {
            if self.reads_ended.load(Ordering::Acquire) {
                return Ok(0);
            }
            match self.down.peek()? {
                None => Ok(0),
                Some(0) => Err(io::ErrorKind::WouldBlock.into()),
                Some(there) => {
                    let len = buf.len().min(there);
                    match self
                        .down
                        .read_by(buf.get_mut(..len).unwrap_or_default(), Some(Instant::now()))
                    {
                        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                            Err(io::ErrorKind::WouldBlock.into())
                        }
                        read => read,
                    }
                }
            }
        }

        /// Waits until `deadline` for something to read, or its end: a read of no bytes.
        pub(super) fn readable_by(&self, deadline: Option<Instant>) -> io::Result<()> {
            match self.down.read_by(&mut [], deadline) {
                Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(()),
                waited => waited.map(drop),
            }
        }

        /// Waits with no clock for something to read, or its end, or for `exit` to be
        /// signalled first: whether it was.
        pub(super) fn readable_or_exited(&self, exit: BorrowedHandle<'_>) -> io::Result<bool> {
            let event = shards_ipc::Event::new()?;
            let handle = self.down.as_handle().as_raw_handle();
            let mut ov = OVERLAPPED {
                hEvent: event.raw(),
                ..OVERLAPPED::default()
            };
            let mut none = 0u8;
            // SAFETY: a read of no bytes into a byte of ours, its OVERLAPPED on this stack,
            // finished or cancelled and waited for below before either goes.
            let started = unsafe { ReadFile(handle, &mut none, 0, std::ptr::null_mut(), &mut ov) };
            // SAFETY: no arguments.
            if started != 0 || unsafe { GetLastError() } != ERROR_IO_PENDING {
                return Ok(false);
            }
            let waited = shards_ipc::await_any(&[], &[event.as_handle(), exit], None);
            let mut moved = 0u32;
            // SAFETY: the read above, cancelled where it is under way still, and waited for.
            unsafe {
                CancelIoEx(handle, &ov);
                GetOverlappedResult(handle, &ov, &mut moved, 1);
            }
            let (_, signalled) = waited?;
            let read = signalled.first().copied().unwrap_or(false);
            Ok(!read && signalled.get(1).copied().unwrap_or(false))
        }

        /// Takes `buf`'s bytes, as many as one write takes, for a write of their own where none
        /// is under way: `WouldBlock` while one is.
        pub(super) fn write_nowait(&self, buf: &[u8]) -> io::Result<usize> {
            let mut w = self.writes();
            settle(&mut w);
            match w.writing {
                Writing::Idle => {}
                Writing::UnderWay => return Err(io::ErrorKind::WouldBlock.into()),
                Writing::Shut => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "its writes are shut down",
                    ));
                }
                Writing::Failed(code) => return Err(error(code)),
            }
            let taken = buf.get(..buf.len().min(self.most)).unwrap_or_default();
            if taken.is_empty() {
                return Ok(0);
            }
            w.out.clear();
            w.out
                .try_reserve_exact(taken.len())
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            w.out.extend_from_slice(taken);
            start(&mut w);
            match w.writing {
                Writing::Failed(code) => Err(error(code)),
                _ => Ok(taken.len()),
            }
        }

        /// The write under way's event, where one is: what a wait for room waits on.
        fn under_way(&self) -> io::Result<Option<shards_ipc::Event>> {
            let mut w = self.writes();
            settle(&mut w);
            match w.writing {
                Writing::UnderWay => w.done.try_clone().map(Some),
                _ => Ok(None),
            }
        }

        /// Waits until `deadline` for the write under way to finish.
        pub(super) fn writable_by(&self, deadline: Option<Instant>) -> io::Result<()> {
            if let Some(done) = self.under_way()? {
                // SAFETY: an event of ours.
                unsafe { WaitForSingleObject(done.raw(), ms(deadline)) };
            }
            Ok(())
        }

        /// Waits with no clock for the write under way to finish, or for `exit` to be
        /// signalled first: whether it was.
        pub(super) fn writable_or_exited(&self, exit: BorrowedHandle<'_>) -> io::Result<bool> {
            let Some(done) = self.under_way()? else {
                return Ok(false);
            };
            let (_, signalled) = shards_ipc::await_any(&[], &[done.as_handle(), exit], None)?;
            let finished = signalled.first().copied().unwrap_or(false);
            Ok(!finished && signalled.get(1).copied().unwrap_or(false))
        }

        /// Ends its writes: the write under way waited for, as its bytes are its peer's before
        /// their end, then the way up closed, which its peer reads as their end.
        pub(super) fn shutdown_write(&self) -> io::Result<()> {
            let mut w = self.writes();
            finish(&mut w);
            w.up = None;
            w.writing = Writing::Shut;
            Ok(())
        }

        /// Ends its reads: each says the end from here on.
        pub(super) fn shutdown_read(&self) {
            self.reads_ended.store(true, Ordering::Release);
        }

        /// Its send and receive buffers: what its way up and its way down hold.
        pub(super) fn buffer_sizes(&self) -> io::Result<(usize, usize)> {
            Ok((self.most, quota(self.down.as_handle(), true)?))
        }

        /// Its pipes' handles, its caller's from here on, its write under way waited for first:
        /// copies, as its clones hold them still; each none where the OS would make none, the
        /// way up's where its writes were shut down.
        pub(super) fn owned(&self) -> crate::OwnedSocket {
            let mut w = self.writes();
            finish(&mut w);
            crate::OwnedSocket {
                down: self.down.as_handle().try_clone_to_owned().ok(),
                up: w.up.as_ref().and_then(|up| up.try_clone().ok()),
            }
        }
    }

    /// A connection's write under way ends before its OVERLAPPED and bytes go: cancelled, as
    /// the connection has ended, and waited for.
    impl Drop for Writes {
        fn drop(&mut self) {
            if let (Writing::UnderWay, Some(up)) = (self.writing, &self.up) {
                let mut moved = 0u32;
                // SAFETY: the OVERLAPPED of the write under way on `up`: cancelled, then
                // waited for (bWait 1) on its event, so the system is done with it and its
                // bytes once this returns.
                unsafe {
                    CancelIoEx(up.as_raw_handle(), self.call.get());
                    GetOverlappedResult(up.as_raw_handle(), self.call.get(), &mut moved, 1);
                }
            }
        }
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
            use std::os::windows::io::AsRawHandle as _;
            let inheritable = |s: crate::Handle<'_>| {
                let mut flags = 0u32;
                // SAFETY: a handle the stream holds open, and a u32 for the call to fill.
                let ok = unsafe {
                    windows_sys::Win32::Foundation::GetHandleInformation(s.as_raw_handle(), &raw mut flags)
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
        let (reads, writes) = {
            let taken = std::os::unix::net::UnixStream::from(socket);
            taken.set_nonblocking(false).unwrap();
            (taken.try_clone().unwrap(), taken)
        };
        // Its pipes (D272): the way up it writes, the way down it reads.
        #[cfg(windows)]
        let (reads, writes) = (
            shards_ipc::Pipe::from_handle(socket.down.unwrap()),
            shards_ipc::Pipe::from_handle(socket.up.unwrap()),
        );
        (&writes).write_all(b"taken").unwrap();
        let mut got = [0u8; 5];
        b.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"taken");
        b.write_all(b"back").unwrap();
        let mut back = [0u8; 4];
        (&reads).read_exact(&mut back).unwrap();
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
