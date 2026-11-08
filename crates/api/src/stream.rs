//! The [`Stream`] every guest connection is: a Unix stream socket whose timeouts and reads
//! wait in poll(2), or on Windows shards' own AF_UNIX socket, which polls them already.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::time::Duration;

use crate::Stream;

/// A connection: [`connect`]ed to a socket, or one end of a [`pair`].
#[derive(Debug)]
pub(crate) struct Socket {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    inner: shards_afunix::UnixStream,
    /// How long a read and a write wait, in ms, 0 for ever: shared by its clones, as a
    /// socket's own timeouts are.
    #[cfg(unix)]
    waits: std::sync::Arc<[std::sync::atomic::AtomicU64; 2]>,
}

/// Connects to the Unix socket at `path`.
pub(crate) fn connect(path: &Path) -> io::Result<Socket> {
    #[cfg(unix)]
    let inner = std::os::unix::net::UnixStream::connect(path)?;
    #[cfg(windows)]
    let inner = shards_afunix::UnixStream::connect(path)?;
    Ok(Socket::new(inner))
}

/// Two connected ends.
pub(crate) fn pair() -> io::Result<(Socket, Socket)> {
    #[cfg(unix)]
    let (a, b) = std::os::unix::net::UnixStream::pair()?;
    #[cfg(windows)]
    let (a, b) = shards_afunix::UnixStream::pair()?;
    Ok((Socket::new(a), Socket::new(b)))
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::Socket;

    const READ: usize = 0;
    const WRITE: usize = 1;

    impl Socket {
        pub(super) fn new(inner: std::os::unix::net::UnixStream) -> Socket {
            Socket {
                inner,
                waits: std::sync::Arc::default(),
            }
        }

        pub(super) fn set_wait(&self, which: usize, timeout: Option<Duration>) -> io::Result<()> {
            let ms = millis(timeout)?;
            if let Some(wait) = self.waits.get(which) {
                wait.store(ms, Ordering::Relaxed);
            }
            Ok(())
        }

        /// Waits until it can be read (`READ`) or written (`WRITE`), or has ended, at most
        /// its timeout: then fails `WouldBlock`, as std's sockets do at theirs on Unix.
        pub(super) fn ready(&self, which: usize) -> io::Result<()> {
            let ms = self.waits.get(which).map_or(0, |w| w.load(Ordering::Relaxed));
            let deadline = (ms != 0)
                .then(|| Instant::now().checked_add(Duration::from_millis(ms)))
                .flatten();
            let events = if which == READ {
                libc::POLLIN
            } else {
                libc::POLLOUT
            };
            loop {
                let wait = match deadline {
                    None => -1,
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                        // Rounded up: a wait of 0 ms returns at once, and would spin.
                        libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
                    }
                };
                let mut fd = libc::pollfd {
                    fd: self.inner.as_raw_fd(),
                    events,
                    revents: 0,
                };
                // SAFETY: poll(2) of one pollfd, of a socket this value holds open.
                let n = unsafe { libc::poll(&raw mut fd, 1, wait) };
                if n > 0 {
                    return Ok(());
                }
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
        }
    }

    impl Socket {
        /// Reads (`READ`) or writes (`WRITE`) once it can, at most its timeout, by `once`
        /// given the socket and `MSG_DONTWAIT`: never waiting in the call itself, so that a
        /// write larger than the room the socket has takes what fits and a later one waits
        /// in poll(2) again, and a read another handle beat it to waits again too, both
        /// within the timeout.
        pub(super) fn transfer(
            &self,
            which: usize,
            mut once: impl FnMut(libc::c_int, libc::c_int) -> libc::ssize_t,
        ) -> io::Result<usize> {
            // A write to a peer that has gone fails EPIPE, not SIGPIPE (Linux; on macOS a
            // Unix socket std connects holds SO_NOSIGPIPE, std's `Socket::new_raw`).
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let flags = libc::MSG_DONTWAIT | if which == WRITE { libc::MSG_NOSIGNAL } else { 0 };
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let flags = libc::MSG_DONTWAIT;
            loop {
                self.ready(which)?;
                let n = once(self.inner.as_raw_fd(), flags);
                if let Ok(n) = usize::try_from(n) {
                    return Ok(n);
                }
                let e = io::Error::last_os_error();
                if !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
                    return Err(e);
                }
            }
        }
    }

    /// A timeout in ms, 0 for none; a zero timeout refused, as std refuses it.
    fn millis(timeout: Option<Duration>) -> io::Result<u64> {
        match timeout {
            None => Ok(0),
            Some(t) if t.is_zero() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot set a 0 duration timeout",
            )),
            Some(t) => Ok(u64::try_from(t.as_millis()).unwrap_or(u64::MAX).max(1)),
        }
    }

    pub(super) const READING: usize = READ;
    pub(super) const WRITING: usize = WRITE;
}

#[cfg(windows)]
impl Socket {
    fn new(inner: shards_afunix::UnixStream) -> Socket {
        Socket { inner }
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Waited for in poll(2), which sees a peer's half-close that a read waiting on a
        // macOS socket can sleep through (PM M236).
        #[cfg(unix)]
        return self.transfer(unix::READING, |fd, flags| {
            // SAFETY: recv(2) into `buf`, writable for its length, from a socket this value
            // holds open.
            unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), flags) }
        });
        #[cfg(windows)]
        (&self.inner).read(buf)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return self.transfer(unix::WRITING, |fd, flags| {
            // SAFETY: send(2) of `buf`, readable for its length, on a socket this value
            // holds open.
            unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), flags) }
        });
        #[cfg(windows)]
        (&self.inner).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stream for Socket {
    fn try_clone(&self) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(Socket {
            inner: self.inner.try_clone()?,
            #[cfg(unix)]
            waits: self.waits.clone(),
        }))
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

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        #[cfg(unix)]
        return self.set_wait(unix::READING, timeout);
        #[cfg(windows)]
        self.inner.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        #[cfg(unix)]
        return self.set_wait(unix::WRITING, timeout);
        #[cfg(windows)]
        self.inner.set_write_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Bytes go both ways, a half-close reaches the peer as its end, and the other way
    /// still carries bytes after it (D12's half-close; Hex HEX-GUEST-9).
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
    /// was: what comes later is read (Hex HEX-GUEST-9).
    #[test]
    fn a_read_times_out_and_the_connection_goes_on() {
        let (mut a, mut b) = pair().unwrap();
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

    /// A write waits for room at most its timeout, however much it is given: one far larger
    /// than the socket's buffers, to a peer that reads none of it, fails there rather than
    /// waiting in the call for room that never comes.
    #[test]
    fn a_write_times_out_however_large() {
        let (mut a, _b) = pair().unwrap();
        Stream::set_write_timeout(&a, Some(Duration::from_millis(50))).unwrap();
        let start = Instant::now();
        let e = a.write_all(&vec![0u8; 64 << 20]).unwrap_err();
        assert!(
            matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut),
            "{e:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
    }

    /// A clone is the same connection: what one writes the peer reads (Hex HEX-GUEST-9).
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
