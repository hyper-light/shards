//! What the network process asks of its host's OS, behind one API: a socket's name as the
//! poller takes it, a TCP connect that does not wait, a Unix connection to a build's proxy,
//! entropy, a socket's buffers, and a doorbell drained. POSIX on Unix; Winsock and BCrypt on
//! Windows (D142), where a doorbell is an event (shards-netring) and Unix sockets are
//! shards-afunix's.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(windows)]
pub use shards_afunix::{UnixListener, UnixStream};

/// A socket as the poller names it: its descriptor, or its Winsock handle.
#[cfg(unix)]
pub type RawSock = std::os::fd::RawFd;
#[cfg(windows)]
pub type RawSock = windows_sys::Win32::Networking::WinSock::SOCKET;

/// A socket's [`RawSock`].
pub trait AsRawSock {
    fn raw_sock(&self) -> RawSock;
}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> AsRawSock for T {
    fn raw_sock(&self) -> RawSock {
        self.as_raw_fd()
    }
}

#[cfg(windows)]
macro_rules! std_sockets {
    ($($t:ty),*) => {$(
        impl AsRawSock for $t {
            fn raw_sock(&self) -> RawSock {
                std::os::windows::io::AsRawSocket::as_raw_socket(self) as RawSock
            }
        }
    )*};
}

#[cfg(windows)]
std_sockets!(TcpStream, std::net::TcpListener, std::net::UdpSocket);

#[cfg(windows)]
impl AsRawSock for UnixStream {
    fn raw_sock(&self) -> RawSock {
        self.raw()
    }
}

#[cfg(windows)]
impl AsRawSock for UnixListener {
    fn raw_sock(&self) -> RawSock {
        self.raw()
    }
}

/// A TCP socket connecting to `to`, of its address's family, without waiting: its connect
/// under way, or done. Nagle is off on the host's leg: the guest's own TCP chose what to
/// send together.
#[cfg(unix)]
pub fn connect_tcp(to: SocketAddr) -> io::Result<TcpStream> {
    use std::os::fd::FromRawFd as _;
    let family = if to.is_ipv6() {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    // SAFETY: socket(2) with constant arguments; the descriptor is owned below.
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made.
    let sock = unsafe { TcpStream::from_raw_fd(fd) };
    sock.set_nonblocking(true)?;
    sock.set_nodelay(true)?;
    // SAFETY: fcntl(2) on our own descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    let r = match to {
        SocketAddr::V4(v4) => {
            let sa = sockaddr_in(v4);
            // SAFETY: a sockaddr_in of its own length.
            unsafe {
                libc::connect(
                    fd,
                    (&raw const sa).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(v6) => {
            let sa = sockaddr_in6(v6);
            // SAFETY: a sockaddr_in6 of its own length.
            unsafe {
                libc::connect(
                    fd,
                    (&raw const sa).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(sock)
}

/// A TCP socket connecting to `to` without waiting (Windows): made as std makes its own,
/// overlapped-capable and inherited by no child, then a connect that would block.
#[cfg(windows)]
pub fn connect_tcp(to: SocketAddr) -> io::Result<TcpStream> {
    connect_tcp_from(to, None)
}

/// A TCP socket connecting to `to` from `to`'s own address, without waiting (Windows): one
/// the host holds as it binds, which binding to it proves (WSAEADDRNOTAVAIL otherwise), the
/// unspecified address refused.
#[cfg(windows)]
pub fn connect_tcp_to_itself(to: SocketAddr) -> io::Result<TcpStream> {
    if to.ip().is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "the unspecified address",
        ));
    }
    connect_tcp_from(to, Some(SocketAddr::new(to.ip(), 0)))
}

#[cfg(windows)]
fn connect_tcp_from(to: SocketAddr, from: Option<SocketAddr>) -> io::Result<TcpStream> {
    use std::os::windows::io::FromRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, INVALID_SOCKET, IPPROTO_TCP, SOCK_STREAM, WSA_FLAG_NO_HANDLE_INHERIT,
        WSA_FLAG_OVERLAPPED, WSAEWOULDBLOCK, WSAGetLastError, WSASocketW, bind, connect,
    };
    let family = if to.is_ipv6() { AF_INET6 } else { AF_INET };
    // SAFETY: plain arguments; the socket is owned below.
    let raw = unsafe {
        WSASocketW(
            i32::from(family),
            SOCK_STREAM,
            IPPROTO_TCP,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if raw == INVALID_SOCKET {
        // SAFETY: no arguments.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    // SAFETY: a socket just made, which nothing else owns.
    let sock = unsafe { TcpStream::from_raw_socket(raw as u64) };
    if let Some(from) = from {
        let (addr, len) = winsock_addr(from);
        // SAFETY: an address of its own length.
        if unsafe { bind(raw, (&raw const addr).cast(), len) } != 0 {
            // SAFETY: no arguments.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
    }
    sock.set_nonblocking(true)?;
    sock.set_nodelay(true)?;
    let (addr, len) = winsock_addr(to);
    // SAFETY: an address of its own length.
    if unsafe { connect(raw, (&raw const addr).cast(), len) } != 0 {
        // SAFETY: no arguments.
        let e = unsafe { WSAGetLastError() };
        if e != WSAEWOULDBLOCK {
            return Err(io::Error::from_raw_os_error(e));
        }
    }
    Ok(sock)
}

/// `to` as Winsock takes it, and its length.
#[cfg(windows)]
pub fn winsock_addr(to: SocketAddr) -> (windows_sys::Win32::Networking::WinSock::SOCKADDR_INET, i32) {
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN, SOCKADDR_IN6,
        SOCKADDR_IN6_0, SOCKADDR_INET,
    };
    match to {
        SocketAddr::V4(v4) => (
            SOCKADDR_INET {
                Ipv4: SOCKADDR_IN {
                    sin_family: AF_INET,
                    sin_port: v4.port().to_be(),
                    sin_addr: IN_ADDR {
                        S_un: IN_ADDR_0 {
                            S_addr: u32::from_ne_bytes(v4.ip().octets()),
                        },
                    },
                    sin_zero: [0; 8],
                },
            },
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ),
        SocketAddr::V6(v6) => (
            SOCKADDR_INET {
                Ipv6: SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: IN6_ADDR {
                        u: IN6_ADDR_0 {
                            Byte: v6.ip().octets(),
                        },
                    },
                    Anonymous: SOCKADDR_IN6_0 {
                        sin6_scope_id: v6.scope_id(),
                    },
                },
            },
            std::mem::size_of::<SOCKADDR_IN6>() as i32,
        ),
    }
}

#[cfg(unix)]
fn sockaddr_in(v4: std::net::SocketAddrV4) -> libc::sockaddr_in {
    // SAFETY: an all-zero sockaddr_in is valid.
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    #[cfg(target_os = "macos")]
    {
        sa.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_port = v4.port().to_be();
    sa.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(v4.ip().octets()),
    };
    sa
}

#[cfg(unix)]
fn sockaddr_in6(v6: std::net::SocketAddrV6) -> libc::sockaddr_in6 {
    // SAFETY: an all-zero sockaddr_in6 is valid.
    let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    #[cfg(target_os = "macos")]
    {
        sa.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
    }
    sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    sa.sin6_port = v6.port().to_be();
    sa.sin6_addr = libc::in6_addr {
        s6_addr: v6.ip().octets(),
    };
    sa
}

/// A Unix connection to `path` that does not wait.
#[cfg(unix)]
pub fn connect_unix(path: &Path) -> io::Result<UnixStream> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    // SAFETY: an all-zero sockaddr_un is valid.
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= sa.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a proxy socket's path longer than sun_path holds",
        ));
    }
    for (d, s) in sa.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    #[cfg(target_os = "macos")]
    {
        sa.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    // SAFETY: socket(2) with constant arguments; the descriptor is owned below.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made.
    let sock = unsafe { UnixStream::from_raw_fd(fd) };
    sock.set_nonblocking(true)?;
    // SAFETY: fcntl(2) on our own descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: a sockaddr_un of its own length.
    let r = unsafe {
        libc::connect(
            fd,
            (&raw const sa).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

/// A Unix connection to `path` that does not wait (Windows).
#[cfg(windows)]
pub fn connect_unix(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect_nonblocking(path)
}

/// `buf` filled from the OS's random source.
pub fn entropy(buf: &mut [u8]) -> io::Result<()> {
    // getrandom(2), which the libc crate binds on glibc and musl alike; it fills a buffer
    // of at most 256 bytes at once once the pool is initialized, but a signal can
    // interrupt it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    loop {
        // SAFETY: getrandom(2) into a buffer of ours, of its length.
        let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
        if usize::try_from(n).is_ok_and(|n| n == buf.len()) {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if n >= 0 || e.kind() != io::ErrorKind::Interrupted {
            return Err(if n >= 0 {
                io::Error::other("getrandom(2) fell short")
            } else {
                e
            });
        }
    }
    // getentropy(2) on macOS, which fills up to 256 bytes or fails.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    {
        // SAFETY: getentropy(2) into a buffer of ours, of its length.
        if unsafe { libc::getentropy(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    // The system's preferred generator (BCryptGenRandom), as std's own seeds come.
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
        let len = u32::try_from(buf.len()).map_err(|_| io::Error::other("too many random bytes at once"))?;
        // SAFETY: a buffer of ours, of its length.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                len,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status < 0 {
            return Err(io::Error::other(format!(
                "BCryptGenRandom: NTSTATUS {:#010x}",
                status as u32
            )));
        }
        Ok(())
    }
}

/// Asks `sock`'s send and receive buffers to be `size` bytes each (SO_SNDBUF, SO_RCVBUF);
/// what the OS gives is its own business.
pub fn set_buffers(sock: RawSock, size: usize) {
    #[cfg(unix)]
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let size = libc::c_int::try_from(size).unwrap_or(libc::c_int::MAX);
        // SAFETY: setsockopt(2) of a socket of the caller's, with a c_int of its length.
        unsafe {
            libc::setsockopt(
                sock,
                libc::SOL_SOCKET,
                option,
                (&raw const size).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::{SO_RCVBUF, SO_SNDBUF, SOL_SOCKET, setsockopt};
        for option in [SO_SNDBUF, SO_RCVBUF] {
            let size = i32::try_from(size).unwrap_or(i32::MAX);
            // SAFETY: setsockopt of a socket of the caller's, with an int of its length.
            unsafe { setsockopt(sock, SOL_SOCKET, option, (&raw const size).cast(), 4) };
        }
    }
}

/// Takes what rang a doorbell: a pipe's bytes on Unix; nothing on Windows, whose event a
/// wait has reset already.
pub fn drain(bell: shards_netring::RawWait) {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 64];
        // SAFETY: a local buffer, from a non-blocking descriptor.
        while unsafe { libc::read(bell, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
    #[cfg(windows)]
    let _ = bell;
}
