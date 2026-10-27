//! Sockets as the drivers that send a run's log over them use them (D163 to D165): a UDP
//! socket connected to its address, and a datagram sent whole, the socket's send buffer
//! grown to it first, as macOS takes none longer (PM M232); and what a TCP connection's
//! kernel knows of what its receiver has acknowledged (PM M233).

use std::io;
use std::net::{TcpStream, ToSocketAddrs as _, UdpSocket};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::time::Duration;

/// A UDP socket connected to `address`, of its first address's family.
pub(super) fn udp(address: &str) -> io::Result<UdpSocket> {
    let to = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("{address}: no address")))?;
    let any: std::net::SocketAddr = if to.is_ipv4() {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = UdpSocket::bind(any)?;
    socket.connect(to)?;
    Ok(socket)
}

/// What sends a datagram on a connected socket: UDP's, or a unix one's.
#[cfg(unix)]
pub(super) trait Datagrams: AsRawFd {
    fn send(&self, data: &[u8]) -> io::Result<usize>;
}

/// What sends a datagram on a connected socket: UDP's, Windows having no unix datagram
/// sockets (AF_UNIX there is SOCK_STREAM's alone).
#[cfg(windows)]
pub(super) trait Datagrams: AsRawSocket {
    fn send(&self, data: &[u8]) -> io::Result<usize>;
}

impl Datagrams for UdpSocket {
    fn send(&self, data: &[u8]) -> io::Result<usize> {
        UdpSocket::send(self, data)
    }
}

#[cfg(unix)]
impl Datagrams for std::os::unix::net::UnixDatagram {
    fn send(&self, data: &[u8]) -> io::Result<usize> {
        std::os::unix::net::UnixDatagram::send(self, data)
    }
}

/// Sends `data` as one datagram on `socket`, its send buffer grown to it first.
pub(super) fn send_datagram(socket: &impl Datagrams, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    fit(socket.as_raw_fd(), data.len());
    #[cfg(windows)]
    fit(socket.as_raw_socket(), data.len());
    let n = socket.send(data)?;
    if n == data.len() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{n} bytes of a {}-byte datagram sent",
            data.len()
        )))
    }
}

/// Lets the datagram socket `socket` send a datagram of `len` bytes, as on macOS: its send
/// buffer raised to it where it is smaller (Winsock's SO_SNDBUF). Whether Windows refuses a
/// datagram longer than it, as macOS does, is not measured yet (the plan's step 8).
#[cfg(windows)]
pub(super) fn fit(socket: RawSocket, len: usize) {
    use windows_sys::Win32::Networking::WinSock::{SO_SNDBUF, SOCKET, SOL_SOCKET, getsockopt, setsockopt};
    let Ok(socket) = SOCKET::try_from(socket) else {
        return;
    };
    let mut have: i32 = 0;
    let mut size = i32::try_from(std::mem::size_of::<i32>()).unwrap_or(4);
    // SAFETY: getsockopt of an int into `have`, `size` its length.
    let read = unsafe {
        getsockopt(
            socket,
            SOL_SOCKET,
            SO_SNDBUF,
            (&raw mut have).cast(),
            &raw mut size,
        )
    };
    if read == 0
        && usize::try_from(have).is_ok_and(|have| have < len)
        && let Ok(want) = i32::try_from(len)
    {
        // SAFETY: setsockopt of an int from `want`. A refusal leaves the send to say so.
        unsafe { setsockopt(socket, SOL_SOCKET, SO_SNDBUF, (&raw const want).cast(), size) };
    }
}

/// Lets the datagram socket `fd` send a datagram of `len` bytes: macOS takes none longer
/// than its send buffer, 9,216 bytes for UDP and 2,048 for a unix socket unless raised,
/// and one exactly as long as it once it is (PM M232). Linux's are larger than any
/// message, and stay as they are.
#[cfg(unix)]
pub(super) fn fit(fd: RawFd, len: usize) {
    let mut have: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt(2) of an int into `have`, `size` its length.
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&raw mut have).cast(),
            &mut size,
        )
    };
    if read == 0
        && usize::try_from(have).is_ok_and(|have| have < len)
        && let Ok(want) = libc::c_int::try_from(len)
    {
        // SAFETY: setsockopt(2) of an int from `want`. A refusal leaves the send to say so.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const want).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// A datagram refused for its size: by this host, or by its receiver's buffer, which on
/// macOS takes none longer than it (ENOBUFS, PM M232).
#[cfg(unix)]
pub(super) fn too_large(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EMSGSIZE | libc::ENOBUFS))
}

/// A datagram refused for its size, as Winsock says it: WSAEMSGSIZE, or WSAENOBUFS.
#[cfg(windows)]
pub(super) fn too_large(e: &io::Error) -> bool {
    use windows_sys::Win32::Networking::WinSock::{WSAEMSGSIZE, WSAENOBUFS};
    matches!(e.raw_os_error(), Some(WSAEMSGSIZE | WSAENOBUFS))
}

/// Of TCP connection `socket`: how many of the bytes written on it its receiver has not
/// acknowledged, its FIN one of them until it is, and how long its kernel waits for an
/// acknowledgement before it sends again; an error where the connection has ended without
/// them, reset or given up (PM M233). Linux: SIOCOUTQ, which counts the FIN, and TCP_INFO
/// (tcp(7)); macOS: TCP_CONNECTION_INFO's bytes in the send buffer, which keeps them until
/// they are acknowledged, its state for the FIN, and its RTO (netinet/tcp.h).
#[cfg(windows)]
pub(super) fn unacknowledged(socket: &TcpStream) -> io::Result<(u64, Duration)> {
    if let Some(e) = socket.take_error()? {
        return Err(e);
    }
    // SIO_TCP_INFO's counts, measured as M233 measured Linux's and macOS's, bring it with
    // the plan's step 8.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "what a TCP connection's receiver has acknowledged is not read on Windows yet",
    ))
}

#[cfg(unix)]
pub(super) fn unacknowledged(socket: &TcpStream) -> io::Result<(u64, Duration)> {
    if let Some(e) = socket.take_error()? {
        return Err(e);
    }
    let fd = socket.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        // Its state first, then what it holds: a connection read as closed holds no more
        // that is ever acknowledged, and its count then is its last. Read the other way, an
        // acknowledgement and the receiver's FIN coming between the two made a connection
        // whose every byte had come seem ended without them: its count from before, its
        // state from after (PM M238).
        // SAFETY: tcp_info is integers, of which all zeros are a value.
        let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
        // SAFETY: getsockopt(2) of TCP_INFO into `info`, `len` its size.
        let read = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                (&raw mut info).cast(),
                &mut len,
            )
        };
        if read != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut queued: libc::c_int = 0;
        // SAFETY: ioctl(2) SIOCOUTQ, whose number is TIOCOUTQ's, of an int into `queued`.
        if unsafe { libc::ioctl(fd, libc::TIOCOUTQ, &raw mut queued) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // TCP_CLOSE (include/net/tcp_states.h): both sides' FINs acknowledged, the
        // connection handed to a TIME_WAIT socket of its own, or reset or given up, which
        // take_error says; bytes unacknowledged in it are never acknowledged now.
        let queued = u64::try_from(queued).unwrap_or(0);
        if info.tcpi_state == 7 && queued > 0 {
            return Err(ended());
        }
        Ok((queued, Duration::from_micros(u64::from(info.tcpi_rto))))
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: tcp_connection_info is integers, of which all zeros are a value.
        let mut info: libc::tcp_connection_info = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::tcp_connection_info>() as libc::socklen_t;
        // SAFETY: getsockopt(2) of TCP_CONNECTION_INFO into `info`, `len` its size.
        let read = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_CONNECTION_INFO,
                (&raw mut info).cast(),
                &mut len,
            )
        };
        if read != 0 {
            return Err(io::Error::last_os_error());
        }
        // netinet/tcp_fsm.h: FIN_WAIT_1, CLOSING and LAST_ACK, a FIN sent and not yet
        // acknowledged; TCPS_CLOSED, reset or given up, which take_error says, and bytes
        // unacknowledged in it never acknowledged now.
        let fin = u64::from(matches!(info.tcpi_state, 6..=8));
        let queued = u64::from(info.tcpi_snd_sbbytes) + fin;
        if info.tcpi_state == 0 && queued > 0 {
            return Err(ended());
        }
        Ok((queued, Duration::from_millis(u64::from(info.tcpi_rto))))
    }
}

#[cfg(unix)]
fn ended() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionReset,
        "the connection ended before its receiver had all written on it",
    )
}
