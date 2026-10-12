//! AF_UNIX stream sockets on Windows (afunix.sys, Windows 10 1803 and later), for what
//! shards carries over Unix sockets on its other hosts: a VM's vsock streams and its
//! builder's sockets. std has Unix sockets on Unix alone; these are the subset shards uses,
//! made as std makes them there: not inherited by children unless handed to one, and
//! blocking until set otherwise. Measured: PM M199 (9-12 GB/s, 3.4-3.7 times loopback
//! TCP's; half-close; scatter-gather; WSAPoll and WSAEventSelect).

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, FIONBIO, INVALID_SOCKET, SD_BOTH, SD_RECEIVE, SD_SEND, SOCK_STREAM, SOCKADDR_UN, SOCKET,
    SOMAXCONN, WSA_FLAG_NO_HANDLE_INHERIT, WSABUF, WSADATA, WSAGetLastError, WSARecv, WSASend, WSASocketW,
    WSAStartup, accept, bind, closesocket, connect, ioctlsocket, listen, recv, send, shutdown,
};

/// A socket, as WSAPoll and WSAEventSelect name it.
pub type RawSocket = SOCKET;

/// Winsock, started once a process (WSAStartup is counted; nothing here ends it).
pub fn startup() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let r = *STARTED.get_or_init(|| {
        let mut data = WSADATA::default();
        // SAFETY: a WSADATA for the call to fill.
        unsafe { WSAStartup(0x0202, &raw mut data) }
    });
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r));
    }
    Ok(())
}

/// The calling thread's last Winsock error.
fn last() -> io::Error {
    // SAFETY: no arguments.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// An owned socket, closed as it drops.
#[derive(Debug)]
struct Socket(SOCKET);

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: a socket this value owns.
        unsafe { closesocket(self.0) };
    }
}

impl Socket {
    /// A new AF_UNIX stream socket, which no child inherits.
    fn new() -> io::Result<Socket> {
        startup()?;
        // SAFETY: plain arguments.
        let s = unsafe {
            WSASocketW(
                i32::from(AF_UNIX),
                SOCK_STREAM,
                0,
                std::ptr::null(),
                0,
                WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if s == INVALID_SOCKET {
            return Err(last());
        }
        Ok(Socket(s))
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        let mut value = u32::from(on);
        // SAFETY: FIONBIO with a u32, on a socket this value owns.
        if unsafe { ioctlsocket(self.0, FIONBIO, &raw mut value) } != 0 {
            return Err(last());
        }
        Ok(())
    }
}

/// `path` as an AF_UNIX address: its UTF-8 bytes, NUL-terminated within the 108 bytes
/// sun_path holds.
fn address(path: &Path) -> io::Result<SOCKADDR_UN> {
    let text = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a socket path that is not UTF-8"))?;
    let mut addr = SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    };
    if text.len() >= addr.sun_path.len() || text.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long for sockaddr_un",
        ));
    }
    for (d, b) in addr.sun_path.iter_mut().zip(text.bytes()) {
        *d = b as i8;
    }
    Ok(addr)
}

const ADDRESS_LEN: i32 = std::mem::size_of::<SOCKADDR_UN>() as i32;

/// A connected AF_UNIX stream.
#[derive(Debug)]
pub struct UnixStream(Socket);

impl super::AsRawSocket for UnixStream {
    fn raw_socket(&self) -> RawSocket {
        self.0.0
    }
}

impl UnixStream {
    /// Connects to the listener at `path`.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<UnixStream> {
        let addr = address(path.as_ref())?;
        let s = Socket::new()?;
        // SAFETY: an address of its own length.
        if unsafe { connect(s.0, (&raw const addr).cast(), ADDRESS_LEN) } != 0 {
            return Err(last());
        }
        Ok(UnixStream(s))
    }

    /// Connects without blocking: a listener whose backlog is full refuses at once.
    pub fn connect_nonblocking(path: &Path) -> io::Result<UnixStream> {
        let addr = address(path)?;
        let s = Socket::new()?;
        s.set_nonblocking(true)?;
        // SAFETY: an address of its own length.
        if unsafe { connect(s.0, (&raw const addr).cast(), ADDRESS_LEN) } != 0 {
            return Err(last());
        }
        Ok(UnixStream(s))
    }

    /// A connected pair: Windows has no socketpair(), so a listener at a fresh path in the
    /// temporary directory, a connect and an accept, the path removed (some 0.6 ms, PM
    /// M199).
    pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // The user's own temporary directory (an AppContainer's, in one), for a file that
        // lives no longer than this call.
        #[expect(
            clippy::disallowed_methods,
            reason = "a pair's listener lives within this call; no test directory"
        )]
        let dir = std::env::temp_dir();
        let mut tries = 0;
        let (listener, path) = loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("shards-pair-{}-{n}.sock", std::process::id()));
            match UnixListener::bind(&path) {
                Ok(l) => break (l, path),
                Err(e) if tries < 8 && e.kind() == io::ErrorKind::AddrInUse => tries += 1,
                Err(e) => return Err(e),
            }
        };
        let ours = UnixStream::connect(&path);
        let theirs = ours.as_ref().ok().map(|_| listener.accept());
        let _ = std::fs::remove_file(&path);
        let ours = ours?;
        match theirs {
            Some(Ok((theirs, ()))) => Ok((ours, theirs)),
            Some(Err(e)) => Err(e),
            None => Err(io::Error::other("the pair's connect failed")),
        }
    }

    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => SD_RECEIVE,
            Shutdown::Write => SD_SEND,
            Shutdown::Both => SD_BOTH,
        };
        // SAFETY: a socket this value owns.
        if unsafe { shutdown(self.0.0, how) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    /// Reads into `bufs`, in order, in one call (WSARecv).
    ///
    /// # Safety
    /// Each `(ptr, len)` is writable for `len` bytes for the call's duration.
    pub unsafe fn readv(&self, bufs: &[(*mut u8, usize)]) -> io::Result<usize> {
        let wsabufs = wsabufs(bufs.iter().map(|&(p, l)| (p, l)));
        let (mut got, mut flags) = (0u32, 0u32);
        // SAFETY: buffers the caller vouches for; synchronous, so they outlive the call.
        let r = unsafe {
            WSARecv(
                self.0.0,
                wsabufs.as_ptr(),
                wsabufs.len() as u32,
                &raw mut got,
                &raw mut flags,
                std::ptr::null_mut(),
                None,
            )
        };
        if r != 0 {
            return Err(last());
        }
        Ok(got as usize)
    }

    /// Writes `bufs`, in order, in one call (WSASend).
    ///
    /// # Safety
    /// Each `(ptr, len)` is readable for `len` bytes for the call's duration.
    pub unsafe fn writev(&self, bufs: &[(*const u8, usize)]) -> io::Result<usize> {
        let wsabufs = wsabufs(bufs.iter().map(|&(p, l)| (p.cast_mut(), l)));
        let mut sent = 0u32;
        // SAFETY: buffers the caller vouches for, only read; synchronous.
        let r = unsafe {
            WSASend(
                self.0.0,
                wsabufs.as_ptr(),
                wsabufs.len() as u32,
                &raw mut sent,
                0,
                std::ptr::null_mut(),
                None,
            )
        };
        if r != 0 {
            return Err(last());
        }
        Ok(sent as usize)
    }
}

/// Buffers as WSABUFs, each at most 4 GiB less one, as a WSABUF's length is 32 bits.
fn wsabufs(bufs: impl Iterator<Item = (*mut u8, usize)>) -> Vec<WSABUF> {
    bufs.map(|(p, l)| WSABUF {
        len: u32::try_from(l).unwrap_or(u32::MAX),
        buf: p,
    })
    .collect()
}

fn read_into(s: SOCKET, buf: &mut [u8]) -> io::Result<usize> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    // SAFETY: `len` bytes of `buf`.
    let n = unsafe { recv(s, buf.as_mut_ptr(), len, 0) };
    usize::try_from(n).map_err(|_| last())
}

fn write_from(s: SOCKET, buf: &[u8]) -> io::Result<usize> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    // SAFETY: `len` bytes of `buf`.
    let n = unsafe { send(s, buf.as_ptr(), len, 0) };
    usize::try_from(n).map_err(|_| last())
}

impl Read for &UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_into(self.0.0, buf)
    }
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_into(self.0.0, buf)
    }
}

impl Write for &UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_from(self.0.0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_from(self.0.0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A listening AF_UNIX socket at a path.
#[derive(Debug)]
pub struct UnixListener(Socket);

impl super::AsRawSocket for UnixListener {
    fn raw_socket(&self) -> RawSocket {
        self.0.0
    }
}

impl UnixListener {
    /// Listens at `path`, which must not exist.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<UnixListener> {
        let addr = address(path.as_ref())?;
        let s = Socket::new()?;
        // SAFETY: an address of its own length; a bound socket.
        unsafe {
            if bind(s.0, (&raw const addr).cast(), ADDRESS_LEN) != 0 {
                return Err(last());
            }
            if listen(s.0, SOMAXCONN as i32) != 0 {
                return Err(last());
            }
        }
        Ok(UnixListener(s))
    }

    /// The next connection, and no address: an AF_UNIX client's is unnamed.
    pub fn accept(&self) -> io::Result<(UnixStream, ())> {
        // SAFETY: a listening socket this value owns; no address asked.
        let s = unsafe { accept(self.0.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        if s == INVALID_SOCKET {
            return Err(last());
        }
        Ok((UnixStream(Socket(s)), ()))
    }

    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A pair carries bytes each way, scatter-gather included; a half-close reads as
    /// end-of-stream while the other way still carries data (PM M199).
    #[test]
    fn a_pair_carries_bytes_and_half_closes() {
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(b"over").unwrap();
        let mut buf = [0u8; 4];
        (&b).read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"over");
        let parts: [&[u8]; 2] = [b"one,", b"two"];
        let bufs: Vec<(*const u8, usize)> = parts.iter().map(|p| (p.as_ptr(), p.len())).collect();
        // SAFETY: two live buffers.
        assert_eq!(unsafe { b.writev(&bufs) }.unwrap(), 7);
        let (mut x, mut y) = ([0u8; 4], [0u8; 3]);
        let mut got = 0;
        while got < 7 {
            let rest: Vec<(*mut u8, usize)> = if got < 4 {
                vec![(x[got..].as_mut_ptr(), 4 - got), (y.as_mut_ptr(), 3)]
            } else {
                vec![(y[got - 4..].as_mut_ptr(), 7 - got)]
            };
            // SAFETY: live buffers of these lengths.
            got += unsafe { a.readv(&rest) }.unwrap();
        }
        assert_eq!((&x, &y), (b"one,", b"two"));
        a.shutdown(Shutdown::Write).unwrap();
        assert_eq!((&b).read(&mut buf).unwrap(), 0);
        (&b).write_all(b"back").unwrap();
        (&a).read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"back");
    }

    /// A non-blocking read with nothing there would block, and says so.
    #[test]
    fn a_nonblocking_read_of_nothing_would_block() {
        let (a, _b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let e = (&a).read(&mut [0u8; 1]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
    }
}
