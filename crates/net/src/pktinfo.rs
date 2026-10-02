//! Datagrams of a published UDP port, each with the host address it came to, and answers
//! sent from that address: what dockerd's userland proxy keeps with `IP_PKTINFO` (moby
//! docker-v29.3.1 portallocator/osallocator_linux.go, bindTCPOrUDP), so that a socket at
//! every address answers a peer from the address it asked, not one the route picks.
//!
//! Both kernels read the source of an answer from the control message's `ipi_spec_dst`
//! when its interface index is 0 (Linux net/ipv4/ip_sockglue.c, ip_cmsg_send; XNU
//! bsd/netinet/udp_usrreq.c, udp_check_pktinfo), and give a datagram's destination in
//! `ipi_addr` (IPv6: `ipi6_addr`, RFC 3542 §6).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, BorrowedFd};

/// Has the UDP socket `fd`, of IPv6 if `v6`, say each datagram's destination.
pub fn enable(fd: BorrowedFd<'_>, v6: bool) -> io::Result<()> {
    let on: libc::c_int = 1;
    let (level, name) = if v6 {
        (libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO)
    } else {
        // Linux's, and macOS's IP_RECVPKTINFO, which is IP_PKTINFO (SDK netinet/in.h).
        (libc::IPPROTO_IP, libc::IP_PKTINFO)
    };
    // SAFETY: setsockopt(2) with a c_int of its length.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (&raw const on).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Room for one pktinfo control message of either family.
const CONTROL: usize = 64;

/// A datagram into `buf`: its length, its sender, and the address it came to, if the
/// kernel said.
pub fn recv(sock: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, Option<IpAddr>)> {
    // SAFETY: all-zero sockaddr_storage and msghdr are valid.
    let mut from: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut control = [0u64; CONTROL / 8];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: as above.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = (&raw mut from).cast();
    msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = CONTROL as _;
    // SAFETY: msg names buffers of the lengths it gives, which outlive the call.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, 0) };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    let peer = sockaddr(&from).ok_or_else(|| io::Error::other("a datagram from no address"))?;
    let mut to = None;
    // SAFETY: walking the control messages recvmsg wrote, within msg_controllen.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&raw const msg);
        while !c.is_null() {
            let data = libc::CMSG_DATA(c);
            if (*c).cmsg_level == libc::IPPROTO_IP && (*c).cmsg_type == libc::IP_PKTINFO {
                let info = data.cast::<libc::in_pktinfo>().read_unaligned();
                to = Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(info.ipi_addr.s_addr))));
            } else if (*c).cmsg_level == libc::IPPROTO_IPV6 && (*c).cmsg_type == libc::IPV6_PKTINFO {
                let info = data.cast::<libc::in6_pktinfo>().read_unaligned();
                to = Some(IpAddr::V6(Ipv6Addr::from(info.ipi6_addr.s6_addr)));
            }
            c = libc::CMSG_NXTHDR(&raw const msg, c);
        }
    }
    Ok((n, peer, to))
}

/// Sends `payload` to `peer`, from host address `from` if given.
pub fn send(sock: &UdpSocket, payload: &[u8], peer: SocketAddr, from: Option<IpAddr>) -> io::Result<usize> {
    let (mut name, namelen) = sockaddr_of(peer);
    let mut control = [0u64; CONTROL / 8];
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr().cast_mut().cast(),
        iov_len: payload.len(),
    };
    // SAFETY: an all-zero msghdr is valid.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = (&raw mut name).cast();
    msg.msg_namelen = namelen;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    if let Some(from) = from {
        msg.msg_control = control.as_mut_ptr().cast();
        // SAFETY: CMSG_SPACE and CMSG_LEN compute sizes; the header written lies within
        // `control`, which holds CONTROL bytes, more than either message's space.
        unsafe {
            let len = match from {
                IpAddr::V4(_) => std::mem::size_of::<libc::in_pktinfo>(),
                IpAddr::V6(_) => std::mem::size_of::<libc::in6_pktinfo>(),
            } as libc::c_uint;
            msg.msg_controllen = libc::CMSG_SPACE(len) as _;
            let c = libc::CMSG_FIRSTHDR(&raw const msg);
            if c.is_null() {
                return Err(io::Error::other("no room for a control message"));
            }
            (*c).cmsg_len = libc::CMSG_LEN(len) as _;
            let data = libc::CMSG_DATA(c);
            match from {
                IpAddr::V4(v4) => {
                    (*c).cmsg_level = libc::IPPROTO_IP;
                    (*c).cmsg_type = libc::IP_PKTINFO;
                    let mut info: libc::in_pktinfo = std::mem::zeroed();
                    info.ipi_spec_dst = libc::in_addr {
                        s_addr: u32::from(v4).to_be(),
                    };
                    data.cast::<libc::in_pktinfo>().write_unaligned(info);
                }
                IpAddr::V6(v6) => {
                    (*c).cmsg_level = libc::IPPROTO_IPV6;
                    (*c).cmsg_type = libc::IPV6_PKTINFO;
                    let mut info: libc::in6_pktinfo = std::mem::zeroed();
                    info.ipi6_addr = libc::in6_addr { s6_addr: v6.octets() };
                    data.cast::<libc::in6_pktinfo>().write_unaligned(info);
                }
            }
        }
    }
    // SAFETY: msg names buffers of the lengths it gives, which outlive the call.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &raw const msg, 0) };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

/// The address in `s`, of IPv4 or IPv6.
fn sockaddr(s: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match libc::c_int::from(s.ss_family) {
        libc::AF_INET => {
            // SAFETY: an AF_INET sockaddr_storage holds a sockaddr_in.
            let sin = unsafe { &*(std::ptr::from_ref(s).cast::<libc::sockaddr_in>()) };
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))),
                u16::from_be(sin.sin_port),
            ))
        }
        libc::AF_INET6 => {
            // SAFETY: an AF_INET6 sockaddr_storage holds a sockaddr_in6.
            let sin6 = unsafe { &*(std::ptr::from_ref(s).cast::<libc::sockaddr_in6>()) };
            Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)),
                u16::from_be(sin6.sin6_port),
            ))
        }
        _ => None,
    }
}

/// `at` as a sockaddr_storage and its length.
pub fn sockaddr_of(at: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero sockaddr_storage is valid.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match at {
        SocketAddr::V4(v4) => {
            // SAFETY: sockaddr_storage holds a sockaddr_in.
            let sin = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in>() };
            #[cfg(target_vendor = "apple")]
            {
                sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
            }
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr = libc::in_addr {
                s_addr: u32::from(*v4.ip()).to_be(),
            };
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(v6) => {
            // SAFETY: sockaddr_storage holds a sockaddr_in6.
            let sin6 = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in6>() };
            #[cfg(target_vendor = "apple")]
            {
                sin6.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
            }
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr = libc::in6_addr {
                s6_addr: v6.ip().octets(),
            };
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (storage, len as libc::socklen_t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A datagram to a socket at every address says which it came to, and the answer
    /// leaves from it: here the host's own address, asked from its loopback, where the
    /// route back to 127.0.0.1 would pick 127.0.0.1.
    #[test]
    fn answers_leave_from_the_address_asked() {
        let Ok(IpAddr::V4(asked)) = UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
            .and_then(|s| s.local_addr())
            .map(|a| a.ip())
        else {
            eprintln!("SKIP: this host has no IPv4 address but its loopback");
            return;
        };
        let server = UdpSocket::bind("0.0.0.0:0").unwrap();
        enable(std::os::fd::AsFd::as_fd(&server), false).unwrap();
        let port = server.local_addr().unwrap().port();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client.send_to(b"ping", (asked, port)).unwrap();
        let mut buf = [0u8; 16];
        let (n, peer, to) = recv(&server, &mut buf).unwrap();
        assert_eq!(
            (&buf[..n], peer, to),
            (
                &b"ping"[..],
                client.local_addr().unwrap(),
                Some(IpAddr::V4(asked))
            )
        );
        send(&server, b"pong", peer, to).unwrap();
        let (n, from) = client.recv_from(&mut buf).unwrap();
        assert_eq!(
            (&buf[..n], from),
            (&b"pong"[..], SocketAddr::new(IpAddr::V4(asked), port))
        );
    }

    #[test]
    fn ipv6_datagrams_say_where_they_came_to() {
        let server = UdpSocket::bind("[::]:0").unwrap();
        enable(std::os::fd::AsFd::as_fd(&server), true).unwrap();
        let port = server.local_addr().unwrap().port();
        let client = UdpSocket::bind("[::1]:0").unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client.send_to(b"ping", (Ipv6Addr::LOCALHOST, port)).unwrap();
        let mut buf = [0u8; 16];
        let (n, peer, to) = recv(&server, &mut buf).unwrap();
        assert_eq!(
            (&buf[..n], to),
            (&b"ping"[..], Some(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        );
        send(&server, b"pong", peer, to).unwrap();
        let (n, from) = client.recv_from(&mut buf).unwrap();
        assert_eq!(
            (&buf[..n], from),
            (
                &b"pong"[..],
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)
            )
        );
    }
}
