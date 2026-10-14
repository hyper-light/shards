//! Datagram sockets as the drivers that send a run's log over them use them (D163, D164):
//! a UDP socket connected to its address, and a datagram sent whole, the socket's send
//! buffer grown to it first, as macOS takes none longer (PM M232).

use std::io;
use std::net::{ToSocketAddrs as _, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};

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
pub(super) trait Datagrams: AsRawFd {
    fn send(&self, data: &[u8]) -> io::Result<usize>;
}

impl Datagrams for UdpSocket {
    fn send(&self, data: &[u8]) -> io::Result<usize> {
        UdpSocket::send(self, data)
    }
}

impl Datagrams for std::os::unix::net::UnixDatagram {
    fn send(&self, data: &[u8]) -> io::Result<usize> {
        std::os::unix::net::UnixDatagram::send(self, data)
    }
}

/// Sends `data` as one datagram on `socket`, its send buffer grown to it first.
pub(super) fn send_datagram(socket: &impl Datagrams, data: &[u8]) -> io::Result<()> {
    fit(socket.as_raw_fd(), data.len());
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

/// Lets the datagram socket `fd` send a datagram of `len` bytes: macOS takes none longer
/// than its send buffer, 9,216 bytes for UDP and 2,048 for a unix socket unless raised,
/// and one exactly as long as it once it is (PM M232). Linux's are larger than any
/// message, and stay as they are.
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
pub(super) fn too_large(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EMSGSIZE | libc::ENOBUFS))
}
