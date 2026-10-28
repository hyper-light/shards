//! AF_UNIX stream sockets on Windows (afunix.sys, Windows 10 1803 and later), for what
//! shards carries over Unix sockets on its other hosts: a VM's vsock streams, its network
//! process's controls and its builder's sockets. std has Unix sockets on Unix alone; these
//! are the subset shards uses, made as std makes them there: not inherited by children
//! unless handed to one, and blocking until set otherwise; overlapped, as std's sockets are
//! made, so that one thread's send never waits for another's read of the same connection
//! (PM M207). Measured: PM M199 (9-12 GB/s, 3.4-3.7 times loopback TCP's; half-close;
//! scatter-gather; WSAPoll and WSAEventSelect), M203 (shared with a confined process) and
//! M207. Nothing on other hosts.

#![cfg(windows)]

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, FIONBIO, FROM_PROTOCOL_INFO, INVALID_SOCKET, POLLRDNORM, POLLWRNORM, SD_BOTH, SD_RECEIVE,
    SD_SEND, SO_ERROR, SOCK_STREAM, SOCKADDR_UN, SOCKET, SOL_SOCKET, SOMAXCONN, WSA_FLAG_NO_HANDLE_INHERIT,
    WSA_FLAG_OVERLAPPED, WSABUF, WSADATA, WSADuplicateSocketW, WSAEWOULDBLOCK, WSAGetLastError, WSAPOLLFD,
    WSAPROTOCOL_INFOW, WSAPoll, WSARecv, WSASend, WSASocketW, WSAStartup, accept, bind, closesocket, connect,
    getpeername, getsockopt, ioctlsocket, listen, recv, send, shutdown,
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

/// How every socket here is made: overlapped, and inherited by no child. Windows serializes
/// the I/O of a handle opened for synchronous I/O, which a socket made without
/// WSA_FLAG_OVERLAPPED is: a send there waits out another thread's WSAPoll or recv on the
/// same connection, by any of its handles, 2.9-3.9 s in M207's runs, where an overlapped
/// socket's goes at once. Its calls without an OVERLAPPED still block as a synchronous
/// socket's do.
const FLAGS: u32 = WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT;

impl Socket {
    /// A new AF_UNIX stream socket, which no child inherits.
    fn new() -> io::Result<Socket> {
        startup()?;
        // SAFETY: plain arguments.
        let s = unsafe { WSASocketW(i32::from(AF_UNIX), SOCK_STREAM, 0, std::ptr::null(), 0, FLAGS) };
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

    /// What process `pid` makes its own copy of this socket from: WSADuplicateSocketW's
    /// protocol info, the way Winsock documents to share a socket, its bytes whole (PM
    /// M203).
    fn share(&self, pid: u32) -> io::Result<Vec<u8>> {
        let mut info = WSAPROTOCOL_INFOW::default();
        // SAFETY: a socket this value owns; a structure for the call to fill.
        if unsafe { WSADuplicateSocketW(self.0, pid, &raw mut info) } != 0 {
            return Err(last());
        }
        // SAFETY: the structure's own bytes, all of them fields (it has no padding).
        let bytes = unsafe { std::slice::from_raw_parts((&raw const info).cast::<u8>(), INFO) };
        Ok(bytes.to_vec())
    }

    /// This process's copy of a socket another process shared with it ([`Socket::share`]).
    fn shared(info: &[u8]) -> io::Result<Socket> {
        startup()?;
        if info.len() != INFO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} bytes of a socket's protocol info, not {INFO}", info.len()),
            ));
        }
        let mut parsed = WSAPROTOCOL_INFOW::default();
        // SAFETY: the structure's own size, from bytes of that length; any bytes are some
        // value of its fields, which Winsock checks.
        unsafe { std::ptr::copy_nonoverlapping(info.as_ptr(), (&raw mut parsed).cast::<u8>(), INFO) };
        // SAFETY: the info as WSADuplicateSocketW wrote it for this process.
        let s = unsafe {
            WSASocketW(
                FROM_PROTOCOL_INFO,
                FROM_PROTOCOL_INFO,
                FROM_PROTOCOL_INFO,
                &raw const parsed,
                0,
                FLAGS,
            )
        };
        if s == INVALID_SOCKET {
            return Err(last());
        }
        Ok(Socket(s))
    }
}

/// WSAPROTOCOL_INFOW's size, which a shared socket's info is.
const INFO: usize = std::mem::size_of::<WSAPROTOCOL_INFOW>();

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
pub struct UnixStream {
    socket: Socket,
    /// How long a read waits, in milliseconds; 0, for ever (`set_read_timeout`).
    read_ms: AtomicU32,
    /// How long a write waits for room, in milliseconds; 0, for ever (`set_write_timeout`).
    write_ms: AtomicU32,
}

impl UnixStream {
    fn wrap(socket: Socket) -> UnixStream {
        UnixStream {
            socket,
            read_ms: AtomicU32::new(0),
            write_ms: AtomicU32::new(0),
        }
    }

    /// A write of `buf`, waiting no longer than the stream's write timeout for room: a
    /// WSAPoll first, as a read polls before it reads (`read_into`).
    fn write_from(&self, buf: &[u8]) -> io::Result<usize> {
        let ms = self.write_ms.load(Ordering::Relaxed);
        if ms != 0 {
            let mut fd = WSAPOLLFD {
                fd: self.socket.0,
                events: POLLWRNORM,
                revents: 0,
            };
            // SAFETY: one entry, a socket this value owns.
            let n = unsafe { WSAPoll(&raw mut fd, 1, i32::try_from(ms).unwrap_or(i32::MAX)) };
            if n < 0 {
                return Err(last());
            }
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no room within the write timeout",
                ));
            }
        }
        write_from(self.socket.0, buf)
    }

    /// A read of what has come into `buf`, waiting no longer than the stream's timeout: a
    /// WSAPoll first, then the read. Not SO_RCVTIMEO, though an overlapped socket keeps it
    /// (PM M207): Winsock leaves a connection whose receive timed out in an indeterminate
    /// state, to be closed, where a poll that times out leaves it as it was.
    fn read_into(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ms = self.read_ms.load(Ordering::Relaxed);
        if ms != 0 {
            let mut fd = WSAPOLLFD {
                fd: self.socket.0,
                events: POLLRDNORM,
                revents: 0,
            };
            // SAFETY: one entry, a socket this value owns.
            let n = unsafe { WSAPoll(&raw mut fd, 1, i32::try_from(ms).unwrap_or(i32::MAX)) };
            if n < 0 {
                return Err(last());
            }
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no data within the read timeout",
                ));
            }
        }
        read_into(self.socket.0, buf)
    }

    /// The socket, as Winsock's calls name it; this value still owns it.
    pub fn raw(&self) -> RawSocket {
        self.socket.0
    }

    /// Connects to the listener at `path`.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<UnixStream> {
        let addr = address(path.as_ref())?;
        let s = Socket::new()?;
        // SAFETY: an address of its own length.
        if unsafe { connect(s.0, (&raw const addr).cast(), ADDRESS_LEN) } != 0 {
            return Err(last());
        }
        Ok(UnixStream::wrap(s))
    }

    /// Connects without blocking on the listener: connected at once, or refused, as Unix's
    /// connect is (a full backlog refuses at once, WSAENOBUFS, Unix's EAGAIN). Windows
    /// says every such connect is under way (WSAEWOULDBLOCK) and ends it in microseconds
    /// where the listener has room (PM M205: 62 µs at most): it is waited out here.
    pub fn connect_nonblocking(path: &Path) -> io::Result<UnixStream> {
        let addr = address(path)?;
        let s = Socket::new()?;
        s.set_nonblocking(true)?;
        // SAFETY: an address of its own length.
        if unsafe { connect(s.0, (&raw const addr).cast(), ADDRESS_LEN) } != 0 {
            let e = last();
            if e.raw_os_error() != Some(WSAEWOULDBLOCK) {
                return Err(e);
            }
            let mut fd = WSAPOLLFD {
                fd: s.0,
                events: POLLWRNORM,
                revents: 0,
            };
            // SAFETY: one entry, a socket this function owns; until the connect ends, which
            // the kernel ends without its listener's process.
            if unsafe { WSAPoll(&raw mut fd, 1, -1) } < 0 {
                return Err(last());
            }
            let stream = UnixStream::wrap(s);
            if let Some(e) = stream.take_error()? {
                return Err(e);
            }
            return Ok(stream);
        }
        Ok(UnixStream::wrap(s))
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
        self.socket.set_nonblocking(on)
    }

    /// A read waits at most `timeout`, or with `None` for ever, as std's does: kept by the
    /// stream and waited out by a poll before each read (`read_into`).
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        let ms: u32 = match timeout {
            None => 0,
            Some(t) if t.is_zero() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot set a 0 duration timeout",
                ));
            }
            Some(t) => u32::try_from(t.as_millis().max(1)).unwrap_or(u32::MAX),
        };
        self.read_ms.store(ms, Ordering::Relaxed);
        Ok(())
    }

    /// A write waits at most `timeout` for room, or with `None` for ever, as std's does:
    /// kept by the stream and waited out by a poll before each write.
    pub fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        let ms: u32 = match timeout {
            None => 0,
            Some(t) if t.is_zero() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot set a 0 duration timeout",
                ));
            }
            Some(t) => u32::try_from(t.as_millis().max(1)).unwrap_or(u32::MAX),
        };
        self.write_ms.store(ms, Ordering::Relaxed);
        Ok(())
    }

    /// Another handle to this connection, as std's `try_clone`: shared with this process.
    pub fn try_clone(&self) -> io::Result<UnixStream> {
        Socket::shared(&self.socket.share(std::process::id())?).map(UnixStream::wrap)
    }

    /// What process `pid` makes its own copy of this connection from, by [`Self::from_shared`]
    /// (PM M203).
    pub fn share_with(&self, pid: u32) -> io::Result<Vec<u8>> {
        self.socket.share(pid)
    }

    /// A connection another process shared with this one ([`Self::share_with`]).
    pub fn from_shared(info: &[u8]) -> io::Result<UnixStream> {
        Socket::shared(info).map(UnixStream::wrap)
    }

    /// The socket's pending error, taken (SO_ERROR), as std's `take_error`.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        let (mut error, mut len) = (0i32, 4i32);
        // SAFETY: an int of its own size, on a socket this value owns.
        if unsafe {
            getsockopt(
                self.socket.0,
                SOL_SOCKET,
                SO_ERROR,
                (&raw mut error).cast(),
                &raw mut len,
            )
        } != 0
        {
            return Err(last());
        }
        Ok((error != 0).then(|| io::Error::from_raw_os_error(error)))
    }

    /// Whether it is connected: its peer named (getpeername); an AF_UNIX peer is unnamed,
    /// and only the connection is told.
    pub fn peer_addr(&self) -> io::Result<()> {
        let mut addr = SOCKADDR_UN {
            sun_family: AF_UNIX,
            sun_path: [0; 108],
        };
        let mut len = ADDRESS_LEN;
        // SAFETY: an address of its own length, on a socket this value owns.
        if unsafe { getpeername(self.socket.0, (&raw mut addr).cast(), &raw mut len) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => SD_RECEIVE,
            Shutdown::Write => SD_SEND,
            Shutdown::Both => SD_BOTH,
        };
        // SAFETY: a socket this value owns.
        if unsafe { shutdown(self.socket.0, how) } != 0 {
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
                self.socket.0,
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
                self.socket.0,
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

impl From<std::os::windows::io::OwnedSocket> for UnixStream {
    /// A connection this process holds already: one another process shared with it, made
    /// its own (WSASocketW of the protocol info its sharer's WSADuplicateSocketW wrote).
    fn from(socket: std::os::windows::io::OwnedSocket) -> UnixStream {
        use std::os::windows::io::IntoRawSocket as _;
        UnixStream::wrap(Socket(socket.into_raw_socket() as SOCKET))
    }
}

impl From<UnixStream> for std::os::windows::io::OwnedSocket {
    /// The connection's socket, to hand to another process (WSADuplicateSocketW) or keep.
    fn from(stream: UnixStream) -> std::os::windows::io::OwnedSocket {
        use std::os::windows::io::FromRawSocket as _;
        let stream = std::mem::ManuallyDrop::new(stream);
        // SAFETY: the socket the stream owned, whose close is forgone with its drop: owned
        // from here alone.
        unsafe { std::os::windows::io::OwnedSocket::from_raw_socket(stream.socket.0 as u64) }
    }
}

impl Read for &UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        UnixStream::read_into(self, buf)
    }
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        UnixStream::read_into(self, buf)
    }
}

impl Write for &UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        UnixStream::write_from(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        UnixStream::write_from(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A listening AF_UNIX socket at a path.
#[derive(Debug)]
pub struct UnixListener(Socket);

impl UnixListener {
    /// The socket, as Winsock's calls name it; this value still owns it.
    pub fn raw(&self) -> RawSocket {
        self.0.0
    }

    /// Listens at `path`, which must not exist.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<UnixListener> {
        let listener = UnixListener::bound(path)?;
        listener.listen()?;
        Ok(listener)
    }

    /// A socket bound at `path`, which must not exist, and not listening yet: a client is
    /// refused until it does ([`Self::listen`]).
    pub fn bound(path: impl AsRef<Path>) -> io::Result<UnixListener> {
        let addr = address(path.as_ref())?;
        let s = Socket::new()?;
        // SAFETY: an address of its own length.
        if unsafe { bind(s.0, (&raw const addr).cast(), ADDRESS_LEN) } != 0 {
            return Err(last());
        }
        Ok(UnixListener(s))
    }

    /// Listens on a socket [`Self::bound`] made.
    pub fn listen(&self) -> io::Result<()> {
        // SAFETY: a bound socket this value owns.
        if unsafe { listen(self.0.0, SOMAXCONN as i32) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    /// What process `pid` makes its own copy of this socket from, by [`Self::from_shared`]
    /// (PM M203).
    pub fn share_with(&self, pid: u32) -> io::Result<Vec<u8>> {
        self.0.share(pid)
    }

    /// A socket another process shared with this one ([`Self::share_with`]).
    pub fn from_shared(info: &[u8]) -> io::Result<UnixListener> {
        Socket::shared(info).map(UnixListener)
    }

    /// Another handle to this socket, as std's `try_clone`: shared with this process.
    pub fn try_clone(&self) -> io::Result<UnixListener> {
        Socket::shared(&self.0.share(std::process::id())?).map(UnixListener)
    }

    /// The next connection, and no address: an AF_UNIX client's is unnamed.
    pub fn accept(&self) -> io::Result<(UnixStream, ())> {
        // SAFETY: a listening socket this value owns; no address asked.
        let s = unsafe { accept(self.0.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        if s == INVALID_SOCKET {
            return Err(last());
        }
        Ok((UnixStream::wrap(Socket(s)), ()))
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

    /// A connection shared by its protocol info is the same connection, and a clone of it
    /// is too; a read waits no longer than its timeout.
    #[test]
    fn a_shared_connection_is_the_same_connection() {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let shared = UnixStream::from_shared(&a.share_with(std::process::id()).unwrap()).unwrap();
        drop(a);
        (&shared).write_all(b"same").unwrap();
        let mut buf = [0u8; 4];
        (&b).read_exact(&mut buf).expect("what the shared copy wrote");
        assert_eq!(&buf, b"same");
        let clone = b.try_clone().unwrap();
        clone
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let t = std::time::Instant::now();
        let e = (&clone).read(&mut buf).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{e:?}");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
        assert!(UnixStream::from_shared(&[0; 3]).is_err());
    }

    /// A send goes at once while another thread waits to read the same connection, with
    /// a timeout or without one, through the socket or a clone of it, as std's sockets'
    /// do (PM M207).
    #[test]
    fn a_send_does_not_wait_for_a_read_in_another_thread() {
        let second = std::time::Duration::from_secs(1);
        for (timeout, by_clone) in [(Some(10), true), (Some(10), false), (None, true), (None, false)] {
            let (a, b) = UnixStream::pair().unwrap();
            a.set_read_timeout(timeout.map(std::time::Duration::from_secs))
                .unwrap();
            b.set_read_timeout(Some(10 * second)).unwrap();
            let clone = a.try_clone().unwrap();
            let sender = if by_clone { &clone } else { &a };
            std::thread::scope(|s| {
                let reader = s.spawn(|| {
                    let mut byte = [0u8; 1];
                    (&a).read(&mut byte).map(|n| (n, byte[0]))
                });
                std::thread::sleep(std::time::Duration::from_millis(100));
                let t = std::time::Instant::now();
                { sender }.write_all(b"x").unwrap();
                let took = t.elapsed();
                let mut got = [0u8; 1];
                (&b).read_exact(&mut got).unwrap();
                let seen = t.elapsed();
                // Ends the read, whatever became of the send.
                (&b).write_all(b"y").unwrap();
                assert_eq!(reader.join().unwrap().unwrap(), (1, b'y'));
                assert!(
                    took < second && seen < second,
                    "timeout {timeout:?}, by a clone {by_clone}: the send took {took:?}, the peer had it after {seen:?}"
                );
            });
        }
    }

    /// A socket bound and not yet listening, shared, listens in its copy, which takes a
    /// client, as a VM's vsock socket does from its spawner (D141).
    #[test]
    fn a_shared_bound_socket_listens_in_its_copy() {
        let dir = shards_testdir::TempDir::new("afunix-shared").unwrap();
        let path = dir.join("s.sock");
        let bound = UnixListener::bound(&path).unwrap();
        let copy = UnixListener::from_shared(&bound.share_with(std::process::id()).unwrap()).unwrap();
        drop(bound);
        assert!(UnixStream::connect(&path).is_err(), "refused until it listens");
        copy.listen().unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (accepted, ()) = copy.accept().unwrap();
        (&client).write_all(b"in").unwrap();
        let mut buf = [0u8; 2];
        (&accepted).read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"in");
    }

    /// A connect that does not wait connects at once where a listener has room, and is
    /// refused where none listens (PM M205).
    #[test]
    fn a_connect_that_does_not_wait_connects_or_is_refused() {
        let dir = shards_testdir::TempDir::new("afunix-nowait").unwrap();
        let path = dir.join("l.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect_nonblocking(&path).unwrap();
        client.peer_addr().expect("connected at once");
        let (accepted, ()) = listener.accept().unwrap();
        (&client).write_all(b"now").unwrap();
        let mut buf = [0u8; 3];
        accepted
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        (&accepted).read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"now");
        drop(listener);
        assert!(UnixStream::connect_nonblocking(&path).is_err(), "nothing listens");
    }

    /// A socket bound and not yet listening refuses a client; listening, it takes one.
    #[test]
    fn a_bound_socket_listens_when_told() {
        let dir = shards_testdir::TempDir::new("afunix-bound").unwrap();
        let path = dir.join("b.sock");
        let bound = UnixListener::bound(&path).unwrap();
        assert!(UnixStream::connect(&path).is_err());
        bound.listen().unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        bound.accept().unwrap();
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
