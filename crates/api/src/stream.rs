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
    /// How long a read and a write wait, in ns, 0 for ever: shared by its clones, as a
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
            let ns = nanos(timeout)?;
            if let Some(wait) = self.waits.get(which) {
                wait.store(ns, Ordering::Relaxed);
            }
            Ok(())
        }

        /// Waits until it can be read (`READ`) or written (`WRITE`), or has ended, at most
        /// its timeout: then fails `WouldBlock`, as std's sockets do at theirs on Unix.
        pub(super) fn ready(&self, which: usize) -> io::Result<()> {
            let ns = self.waits.get(which).map_or(0, |w| w.load(Ordering::Relaxed));
            let deadline = (ns != 0)
                .then(|| Instant::now().checked_add(Duration::from_nanos(ns)))
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
                        // In poll(2)'s unit, ms (poll(2): "timeout specifies the number of
                        // milliseconds"), rounded up: a wait of 0 returns at once, and would
                        // spin until the deadline.
                        libc::c_int::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(libc::c_int::MAX)
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

    /// A timeout in ns, 0 for none: a `Duration`'s own unit, so any timeout is kept as
    /// given (a nonzero one is at least 1 ns), and one past u64's 584 years is for ever's
    /// equal. A zero timeout is refused, as std refuses it (`UnixStream::set_read_timeout`).
    pub(super) fn nanos(timeout: Option<Duration>) -> io::Result<u64> {
        match timeout {
            None => Ok(0),
            Some(t) if t.is_zero() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot set a 0 duration timeout",
            )),
            Some(t) => Ok(u64::try_from(t.as_nanos()).unwrap_or(u64::MAX)),
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
        self.ready(unix::READING)?;
        (&self.inner).read(buf)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        self.ready(unix::WRITING)?;
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

    /// A timeout is kept as given, to its nanosecond: none rounded down to a shorter one or
    /// up to poll(2)'s millisecond, and none of them for ever (Hex HEX-GUEST-9).
    #[cfg(unix)]
    #[test]
    fn a_timeout_is_kept_as_given() {
        assert_eq!(unix::nanos(Some(Duration::from_micros(1500))).unwrap(), 1_500_000);
        assert_eq!(unix::nanos(Some(Duration::from_nanos(1))).unwrap(), 1);
        assert_eq!(unix::nanos(None).unwrap(), 0);
        assert!(unix::nanos(Some(Duration::ZERO)).is_err());
        assert_eq!(unix::nanos(Some(Duration::MAX)).unwrap(), u64::MAX);
    }

    /// How long a timed operation here may take: its timeout, `UNSERVED_PATIENCE`, and
    /// this host class's recorded lateness of a timed wait (`OVERSHOOT`), a hang guard.
    fn timed() -> (Duration, Duration) {
        let late = crate::conformance::OVERSHOOT.unwrap_or_else(|pending| panic!("{pending}"));
        let timeout = crate::conformance::UNSERVED_PATIENCE;
        (timeout, timeout.saturating_add(late))
    }

    /// A write to a peer that stops reading fails at its timeout, having moved what the
    /// socket took, rather than blocking for ever (Hex HEX-GUEST-9; review B3).
    #[test]
    fn a_write_to_a_peer_that_stops_reading_ends_at_its_timeout() {
        let (timeout, guard) = timed();
        let (mut a, _b) = pair().unwrap();
        Stream::set_write_timeout(&a, Some(timeout)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // An input: more than any socket's buffer holds (macOS's 8 KiB, Linux's
            // wmem_max), so the peer's not reading is what stops it.
            let big = vec![7u8; 64 << 20];
            tx.send(a.write_all(&big).map_err(|e| e.kind()))
        });
        let wrote = rx
            .recv_timeout(guard)
            .expect("the write blocked past its timeout");
        assert!(
            matches!(wrote, Err(io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "{wrote:?}"
        );
    }

    /// Two clones reading one byte: one reads it, the other fails at its timeout rather
    /// than blocking after a poll said the byte was there (Hex HEX-GUEST-9; review B3).
    #[test]
    fn a_read_a_clone_raced_ends_at_its_timeout() {
        let (timeout, guard) = timed();
        let (a, mut b) = pair().unwrap();
        Stream::set_read_timeout(&a, Some(timeout)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..2 {
            let mut reader = Stream::try_clone(&a).unwrap();
            let tx = tx.clone();
            std::thread::spawn(move || tx.send(reader.read(&mut [0u8; 1]).map_err(|e| e.kind())));
        }
        b.write_all(b"x").unwrap();
        let mut got: Vec<_> = (0..2)
            .map(|_| rx.recv_timeout(guard).expect("a read blocked past its timeout"))
            .collect();
        got.sort_by_key(|r| r.is_err());
        assert_eq!(got[0], Ok(1));
        assert!(
            matches!(got[1], Err(io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "{got:?}"
        );
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
