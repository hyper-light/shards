//! `run -p` and `-P` as dockerd publishes a container's ports (moby docker-v29.3.1,
//! daemon/libnetwork/portallocator/osallocator_linux.go; measured against Docker Desktop's
//! dockerd 29.3.1, 2026-10-02): each binding bound on the host as the run starts, here by
//! the daemon, which hands the listening sockets to the VM's network process (D31); that
//! process opens each connection they take to the guest. A binding that cannot be bound
//! fails the start, the container left created.
//!
//! Unlike dockerd: a host port range is tried port by port to its end, where dockerd
//! gives up after 10 tries; and `-P` binds IPv6 too, as `-p` with no address does, where
//! Docker Desktop's bound IPv4 alone.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{FromRawFd as _, OwnedFd};

use shards_ipc::{Publish, Run};

use crate::containers::PortRecord;

/// What a run publishes, bound: the listening sockets with the guest port each reaches,
/// and the ports its container lists.
#[derive(Debug, Default)]
pub(super) struct Bound {
    pub listeners: Vec<(OwnedFd, u16)>,
    pub ports: Vec<PortRecord>,
}

/// Host addresses a container publishes: held from their binding until the network
/// process of the VM that took them (`vm`) has gone, with the daemon's copies of their
/// listening sockets once that VM has its run.
#[derive(Debug)]
pub(super) struct Held {
    pub container: String,
    pub vm: Option<u32>,
    pub at: Vec<SocketAddr>,
    pub listeners: Vec<OwnedFd>,
}

/// Whose a host address found in use is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InUse {
    /// Another program's.
    Host,
    /// A running container's of this daemon.
    Allocated,
    /// A run's that has ended, and is free now.
    Freed,
}

/// Where listening socket `fd` is bound.
pub(super) fn local_addr(fd: std::os::fd::BorrowedFd<'_>) -> Option<SocketAddr> {
    let fd = fd.try_clone_to_owned().ok()?;
    std::net::TcpListener::from(fd).local_addr().ok()
}

/// A port as an image config's `ExposedPorts` key names it: `80/tcp`, or `80` for TCP.
fn exposed(key: &str) -> Option<(u16, String)> {
    let (port, proto) = key.split_once('/').unwrap_or((key, "tcp"));
    Some((
        port.parse().ok()?,
        if proto.is_empty() {
            "tcp".into()
        } else {
            proto.to_lowercase()
        },
    ))
}

/// The bindings `run` asks for: `-p`'s, then with `-P` one to a port the host picks for
/// each port exposed (the image's and `-p`'s) that has none. And the ports exposed alone,
/// which its container lists without a host side.
pub(super) fn wanted(run: &Run, image_exposed: &[String]) -> (Vec<Publish>, Vec<(u16, String)>) {
    let mut ports: Vec<(u16, String)> = image_exposed.iter().filter_map(|k| exposed(k)).collect();
    for p in &run.publish {
        let port = (p.port, p.proto.clone());
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    let mut bindings = run.publish.clone();
    if run.publish_all {
        for (port, proto) in &ports {
            if !run.publish.iter().any(|p| p.port == *port && p.proto == *proto) {
                bindings.push(Publish {
                    port: *port,
                    proto: proto.clone(),
                    host_ip: String::new(),
                    host_port: String::new(),
                });
            }
        }
    }
    let alone = ports
        .into_iter()
        .filter(|(port, proto)| !bindings.iter().any(|b| b.port == *port && b.proto == *proto))
        .collect();
    (bindings, alone)
}

/// What shards cannot publish yet, refused before a container is made for it.
pub(super) fn unsupported(bindings: &[Publish]) -> Option<String> {
    bindings
        .iter()
        .find(|b| b.proto != "tcp")
        .map(|b| format!("\"-p {}/{}\" is not supported by shards yet", b.port, b.proto))
}

/// Binds `bindings`, and lists them with the ports exposed `alone`. Whose an address in
/// use is, `in_use` says, waiting for one a run of this daemon has just let go of.
pub(super) fn bind(
    bindings: &[Publish],
    alone: &[(u16, String)],
    in_use: impl Fn(SocketAddr) -> InUse,
) -> Result<Bound, String> {
    let mut bound = Bound::default();
    for b in bindings {
        // No address is every one: IPv4's and IPv6's, on one port.
        let ips: Vec<IpAddr> = if b.host_ip.is_empty() {
            vec![
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            ]
        } else {
            vec![
                b.host_ip
                    .parse()
                    .map_err(|_| format!("invalid host address {}", b.host_ip))?,
            ]
        };
        let (listeners, port) = bind_one(&ips, &b.host_port, &in_use)?;
        for (ip, fd) in ips.iter().zip(listeners) {
            bound.listeners.push((fd, b.port));
            bound.ports.push(PortRecord {
                ip: Some(*ip),
                private: b.port,
                public: port,
                proto: b.proto.clone(),
            });
        }
    }
    for (port, proto) in alone {
        bound.ports.push(PortRecord {
            ip: None,
            private: *port,
            public: 0,
            proto: proto.clone(),
        });
    }
    Ok(bound)
}

/// Listening sockets at each of `ips` on one host port: `host_port`'s, the first free
/// one of its range, or one the kernel picks for the first address.
fn bind_one(
    ips: &[IpAddr],
    host_port: &str,
    in_use: &dyn Fn(SocketAddr) -> InUse,
) -> Result<(Vec<OwnedFd>, u16), String> {
    let range = match host_port.split_once('-') {
        Some((a, b)) => (parse_port(a, host_port)?, parse_port(b, host_port)?),
        None if host_port.is_empty() => (0, 0),
        None => {
            let p = parse_port(host_port, host_port)?;
            (p, p)
        }
    };
    let mut last_err = String::new();
    for port in range.0..=range.1 {
        match bind_all(ips, port, in_use, &listen) {
            Ok(bound) => return Ok(bound),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn parse_port(s: &str, whole: &str) -> Result<u16, String> {
    s.parse().map_err(|_| format!("invalid host port {whole}"))
}

/// `port` at every one of `ips`, or the first that fails. Port 0 is the kernel's pick at
/// the first address, then the same at the rest; one taken there is picked afresh, up to
/// dockerd's 10 tries (maxAllocateAttempts). An address a run has just let go of is
/// tried again, once.
fn bind_all(
    ips: &[IpAddr],
    port: u16,
    in_use: &dyn Fn(SocketAddr) -> InUse,
    listen: &dyn Fn(SocketAddr) -> std::io::Result<(OwnedFd, u16)>,
) -> Result<(Vec<OwnedFd>, u16), String> {
    const TRIES: u32 = 10;
    let mut tries = 1;
    let mut freed = false;
    loop {
        let mut fds = Vec::with_capacity(ips.len());
        let mut chosen = port;
        let mut failed = None;
        for ip in ips {
            match listen(SocketAddr::new(*ip, chosen)) {
                Ok((fd, at)) => {
                    chosen = at;
                    fds.push(fd);
                }
                Err(e) => {
                    failed = Some((ip, e));
                    break;
                }
            }
        }
        match failed {
            None => return Ok((fds, chosen)),
            // The kernel's pick at IPv4 is taken at IPv6: another.
            Some((_, e)) if port == 0 && e.kind() == std::io::ErrorKind::AddrInUse && tries < TRIES => {
                tries += 1;
            }
            // dockerd's allocator's words for its own containers' ports, net.IP's address
            // and the port; and bindTCPOrUDP's for the host's, netip.AddrPort's.
            Some((ip, e)) if e.kind() == std::io::ErrorKind::AddrInUse && !freed => {
                match in_use(SocketAddr::new(*ip, chosen)) {
                    InUse::Allocated => {
                        return Err(format!(
                            "Bind for {ip}:{chosen} failed: port is already allocated"
                        ));
                    }
                    InUse::Freed => freed = true,
                    InUse::Host => {
                        return Err(format!(
                            "failed to bind host port {}/tcp: {}",
                            SocketAddr::new(*ip, chosen),
                            go_error(&e)
                        ));
                    }
                }
            }
            Some((ip, e)) => {
                return Err(format!(
                    "failed to bind host port {}/tcp: {}",
                    SocketAddr::new(*ip, chosen),
                    go_error(&e)
                ));
            }
        }
    }
}

/// An OS error in Go's words (syscall.Errno.Error): Linux's from Go's own table, other
/// hosts' as their strerror, which Go's tables copy, lower-cased.
fn go_error(e: &std::io::Error) -> String {
    let Some(errno) = e.raw_os_error() else {
        return e.to_string();
    };
    if cfg!(target_os = "linux") {
        return shards_cmdline::go::linux_error(errno);
    }
    let text = e.to_string();
    let text = text
        .strip_suffix(&format!(" (os error {errno})"))
        .unwrap_or(&text);
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

/// A TCP socket listening at `at`, and the port it has: close-on-exec, nonblocking for
/// the network process, address reuse as Go's net.Listen sets it, and IPv6 alone on an
/// IPv6 address, so that IPv4's own socket at the same port is not refused.
fn listen(at: SocketAddr) -> std::io::Result<(OwnedFd, u16)> {
    let family = if at.is_ipv6() {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, ours alone.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
    let on: libc::c_int = 1;
    let len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: fcntl(2) and setsockopt(2) on our descriptor, with a c_int of its length.
    unsafe {
        libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        libc::setsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&raw const on).cast(),
            len,
        );
        if at.is_ipv6() {
            libc::setsockopt(
                raw,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                (&raw const on).cast(),
                len,
            );
        }
    }
    let (storage, slen) = sockaddr(at);
    // SAFETY: a sockaddr of its own length.
    if unsafe { libc::bind(raw, (&raw const storage).cast(), slen) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: listen(2) on our bound socket.
    if unsafe { libc::listen(raw, libc::SOMAXCONN) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let listener = std::net::TcpListener::from(fd);
    let port = listener.local_addr()?.port();
    Ok((OwnedFd::from(listener), port))
}

/// `at` as a sockaddr_storage and its length.
fn sockaddr(at: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
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
                s_addr: u32::from_ne_bytes(v4.ip().octets()),
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

    fn run(publish: &[(u16, &str, &str)], all: bool) -> Run {
        Run {
            publish: publish
                .iter()
                .map(|(port, ip, host)| Publish {
                    port: *port,
                    proto: "tcp".into(),
                    host_ip: (*ip).into(),
                    host_port: (*host).into(),
                })
                .collect(),
            publish_all: all,
            ..Run::default()
        }
    }

    #[test]
    fn publish_all_binds_every_exposed_port_without_a_binding() {
        let image = ["90/tcp".to_string(), "53/udp".to_string(), "91".to_string()];
        let (bindings, alone) = wanted(&run(&[(91, "", "9191")], true), &image);
        let shown: Vec<(u16, &str, &str)> = bindings
            .iter()
            .map(|b| (b.port, b.proto.as_str(), b.host_port.as_str()))
            .collect();
        assert_eq!(shown, [(91, "tcp", "9191"), (90, "tcp", ""), (53, "udp", "")]);
        assert!(alone.is_empty());
        let (bindings, alone) = wanted(&run(&[], false), &image);
        assert!(bindings.is_empty());
        assert_eq!(
            alone,
            [(90, "tcp".into()), (53, "udp".into()), (91, "tcp".into())]
        );
    }

    /// A port in use is refused in dockerd's words, its allocator's for a container's;
    /// one the kernel picks is the same at IPv4 and IPv6; a range takes its first free
    /// port.
    #[test]
    fn ports_bind_as_dockerd_binds_them() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let loopback = [IpAddr::V4(Ipv4Addr::LOCALHOST)];
        let host = |_| InUse::Host;
        assert_eq!(
            bind_one(&loopback, &port.to_string(), &host).unwrap_err(),
            format!("failed to bind host port 127.0.0.1:{port}/tcp: address already in use")
        );
        assert_eq!(
            bind_one(&loopback, &port.to_string(), &|_| InUse::Allocated).unwrap_err(),
            format!("Bind for 127.0.0.1:{port} failed: port is already allocated")
        );
        // An address let go of meanwhile is bound.
        let holder = std::cell::RefCell::new(Some(taken));
        let release = |at: SocketAddr| {
            assert_eq!(at, SocketAddr::new(loopback[0], port));
            holder.borrow_mut().take();
            InUse::Freed
        };
        assert_eq!(bind_one(&loopback, &port.to_string(), &release).unwrap().1, port);
        let both = [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];
        let (fds, picked) = bind_one(&both, "", &host).unwrap();
        assert_eq!(fds.len(), 2);
        assert_ne!(picked, 0);
        let v6 = std::net::TcpListener::bind("[::1]:0").unwrap();
        let v6_port = v6.local_addr().unwrap().port();
        assert_eq!(
            bind_one(&both, &v6_port.to_string(), &host).unwrap_err(),
            format!("failed to bind host port [::1]:{v6_port}/tcp: address already in use")
        );
        let blocker = std::net::TcpListener::bind((loopback[0], port)).unwrap();
        let (_, next) = bind_one(&loopback, &format!("{port}-{}", port.saturating_add(50)), &host).unwrap();
        assert!(next > port && next <= port.saturating_add(50), "{next}");
        drop(blocker);
    }

    /// A port the kernel picks at IPv4 that IPv6 has taken is picked afresh, up to
    /// dockerd's 10 tries; a port asked for is not.
    #[test]
    fn a_picked_port_taken_at_ipv6_is_picked_again() {
        let both = [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];
        let refusals = &std::cell::Cell::new(0);
        let refusing = |n: u32| {
            move |at: SocketAddr| {
                if at.is_ipv6() && refusals.get() < n {
                    refusals.set(refusals.get() + 1);
                    return Err(std::io::ErrorKind::AddrInUse.into());
                }
                listen(at)
            }
        };
        let (fds, port) = bind_all(&both, 0, &|_| InUse::Host, &refusing(9)).unwrap();
        assert_eq!((fds.len(), refusals.get()), (2, 9));
        assert_ne!(port, 0);
        refusals.set(0);
        assert!(bind_all(&both, 0, &|_| InUse::Host, &refusing(10)).is_err());
        refusals.set(0);
        let asked = listen(SocketAddr::new(both[0], 0)).unwrap().1;
        assert!(bind_all(&both, asked, &|_| InUse::Host, &refusing(1)).is_err());
        assert_eq!(refusals.get(), 1);
    }

    /// An IPv6 listener takes IPv6 alone: IPv4's connections are not its.
    #[test]
    fn ipv6_listeners_take_no_ipv4() {
        let (_v6, port) = listen(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).unwrap();
        assert!(std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
        assert!(std::net::TcpStream::connect((Ipv6Addr::LOCALHOST, port)).is_ok());
    }

    #[test]
    fn non_tcp_ports_are_refused_until_shards_carries_them() {
        let udp = Publish {
            port: 53,
            proto: "udp".into(),
            ..Publish::default()
        };
        assert_eq!(
            unsupported(&[udp]).unwrap(),
            "\"-p 53/udp\" is not supported by shards yet"
        );
    }
}
