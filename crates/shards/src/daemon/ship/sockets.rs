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

/// What a connection's SIO_TCP_INFO has counted of the bytes it sent again: BytesRetrans
/// is 32 bits, so each read's rise since the last is added, past its wraps (Windows).
#[cfg(windows)]
#[derive(Debug, Default)]
pub(super) struct Retransmitted {
    last: u32,
    total: u64,
}

#[cfg(windows)]
impl Retransmitted {
    /// All its connection has sent again, its count read now as `read`.
    fn now(&mut self, read: u32) -> u64 {
        self.total = self.total.saturating_add(u64::from(read.wrapping_sub(self.last)));
        self.last = read;
        self.total
    }
}

/// Of TCP connection `socket`, `written` bytes written on it: how many its receiver has
/// not acknowledged, its FIN one of them until it is, and no RTO, which Windows does not
/// say (its caller waits RFC 6298's least); an error where the connection has ended
/// without them, reset or given up. SIO_TCP_INFO's counts (PM M373): BytesOut counts the
/// bytes sent again too (`retransmitted` keeps its whole count), so those acknowledged are
/// `BytesOut − BytesRetrans − BytesInFlight`, and the bytes written less them are the
/// receiver's to acknowledge, those held unsent among them; the FIN while the state is
/// FIN_WAIT_1, CLOSING or LAST_ACK, as on macOS.
#[cfg(windows)]
pub(super) fn unacknowledged(
    socket: &TcpStream,
    written: u64,
    retransmitted: &mut Retransmitted,
) -> io::Result<(u64, Duration)> {
    use windows_sys::Win32::Networking::WinSock::{
        SIO_TCP_INFO, SOCKET, TCP_INFO_v0, TCPSTATE_CLOSED, TCPSTATE_CLOSING, TCPSTATE_FIN_WAIT_1,
        TCPSTATE_LAST_ACK, WSAIoctl,
    };
    if let Some(e) = socket.take_error()? {
        return Err(e);
    }
    let raw = SOCKET::try_from(socket.as_raw_socket())
        .map_err(|_| io::Error::other("a socket's value past SOCKET's range"))?;
    let version: u32 = 0;
    let mut info = TCP_INFO_v0::default();
    let size = u32::try_from(std::mem::size_of::<TCP_INFO_v0>()).unwrap_or(u32::MAX);
    let mut got = 0u32;
    // SAFETY: SIO_TCP_INFO of version 0 into `info`, its size given, synchronous: no
    // OVERLAPPED, no completion routine.
    let asked = unsafe {
        WSAIoctl(
            raw,
            SIO_TCP_INFO,
            (&raw const version).cast(),
            4,
            (&raw mut info).cast(),
            size,
            &raw mut got,
            std::ptr::null_mut(),
            None,
        )
    };
    if asked != 0 {
        return Err(io::Error::last_os_error());
    }
    let acknowledged = info
        .BytesOut
        .saturating_sub(retransmitted.now(info.BytesRetrans))
        .saturating_sub(u64::from(info.BytesInFlight));
    let fin = u64::from(matches!(
        info.State,
        TCPSTATE_FIN_WAIT_1 | TCPSTATE_CLOSING | TCPSTATE_LAST_ACK
    ));
    let queued = written.saturating_sub(acknowledged).saturating_add(fin);
    // Reset or given up, which take_error says; or both sides' FINs acknowledged: bytes
    // unacknowledged in it never are now.
    if info.State == TCPSTATE_CLOSED && queued > 0 {
        return Err(ended());
    }
    Ok((queued, Duration::ZERO))
}

/// Of TCP connection `socket`: how many of the bytes written on it its receiver has not
/// acknowledged, its FIN one of them until it is, and how long its kernel waits for an
/// acknowledgement before it sends again; an error where the connection has ended without
/// them, reset or given up (PM M233). Linux: SIOCOUTQ, which counts the FIN, and TCP_INFO
/// (tcp(7)); macOS: TCP_CONNECTION_INFO's bytes in the send buffer, which keeps them until
/// they are acknowledged, its state for the FIN, and its RTO (netinet/tcp.h).
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

fn ended() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionReset,
        "the connection ended before its receiver had all written on it",
    )
}
