//! A run on the host's network (`--network host`, D124): its guest's binds made the
//! host's. Init's process for them (shards-init `hostbind.rs`) asks, by the run's VM, for
//! each bind of the run's processes to be made on the host, and for its listen; here each
//! is a socket of the host's, bound with the guest's socket's options at the host's
//! address of the kind the guest bound, and listening once the guest's does, when it goes
//! to the VM's network process, which passes what comes to it on to the guest
//! (`NET_PUBLISHED`). The daemon keeps its own copy of each until the guest's socket goes
//! (`UNBIND`), or the run ends. That process answers each port's letting go once it has
//! closed its copies, which a bind of the same port, refused meanwhile, waits for: a server
//! that closes its socket and binds its port again at once has it, as on the host.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use shards_abi::run::{HostBind, HostBound, HostListen, kind};
use shards_vmm::devices::virtio::fs::host::linux_errno;

/// IP protocol numbers (IANA).
const TCP: u8 = 6;
const UDP: u8 = 17;
/// Linux's errnos for what is the daemon's to say: a socket it does not have, and a
/// network process that is gone.
const EBADF: i32 = 9;
const ENETDOWN: i32 = 100;
const EADDRINUSE: i32 = 98;

/// A run's sockets on the host's network: its VM's network process, what has come of its
/// answers, and how many of them are owed; each socket by its number, with its protocol
/// and whether that process has it.
#[derive(Debug)]
pub(super) struct HostNet {
    net: UnixStream,
    incoming: shards_ipc::Incoming,
    owed: u32,
    patience: Duration,
    sockets: HashMap<u32, (OwnedFd, u8, bool)>,
    next: u32,
}

impl HostNet {
    /// What it sends the network process, and waits for of it, on the followers' loop,
    /// waits `patience` at most, as a run's start waits for that process's answers.
    pub(super) fn new(net: UnixStream, patience: Duration) -> HostNet {
        let _ = net.set_write_timeout(Some(patience));
        HostNet {
            net,
            incoming: shards_ipc::Incoming::default(),
            owed: 0,
            patience,
            sockets: HashMap::new(),
            next: 0,
        }
    }

    /// The guest's ask, its frame's kind then its payload: the answer it has
    /// (`HOST_BOUND`'s payload), where it has one.
    pub(super) fn ask(&mut self, frame: &[u8]) -> Option<[u8; 14]> {
        self.answers(false);
        let (&which, payload) = frame.split_first()?;
        match which {
            kind::BIND => Some(self.bind(&HostBind::decode(payload)?).encode()),
            kind::LISTEN => Some(self.listen(&HostListen::decode(payload)?).encode()),
            kind::UNBIND => {
                self.unbind(u32::from_be_bytes(payload.try_into().ok()?));
                None
            }
            _ => None,
        }
    }

    /// Takes the network process's answers to `NET_UNPUBLISHED` that have come; with
    /// `wait`, every one owed, waiting [`patience`](Self::new) at most.
    fn answers(&mut self, wait: bool) {
        let deadline = Instant::now() + self.patience;
        while self.owed > 0 {
            match self.incoming.take(&self.net) {
                Ok(shards_ipc::Took::Message(m)) => {
                    if m.kind == shards_ipc::kind::NET_UNPUBLISHED {
                        self.owed -= 1;
                    }
                }
                Ok(shards_ipc::Took::Partial) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if !wait || left.is_zero() || !readable(&self.net, left) {
                        return;
                    }
                }
                Ok(shards_ipc::Took::Ended) | Err(_) => {
                    self.owed = 0;
                    return;
                }
            }
        }
    }

    fn bind(&mut self, b: &HostBind) -> HostBound {
        // A port the network process is letting go of is free once it has: waited for,
        // where the bind finds it held.
        let made = match socket(b) {
            Err(EADDRINUSE) if self.owed > 0 => {
                self.answers(true);
                socket(b)
            }
            made => made,
        };
        match made {
            Ok((fd, port)) => {
                let number = self.next;
                self.next = self.next.wrapping_add(1);
                self.sockets.insert(number, (fd, b.proto, false));
                HostBound {
                    request: b.request,
                    errno: 0,
                    port,
                    number,
                }
            }
            Err(errno) => HostBound {
                request: b.request,
                errno,
                port: 0,
                number: 0,
            },
        }
    }

    /// Socket `l.number` listening, a TCP socket's, and what comes to it going to the
    /// guest's port, by the network process.
    fn listen(&mut self, l: &HostListen) -> HostBound {
        let answer = |errno| HostBound {
            request: l.request,
            errno,
            port: 0,
            number: l.number,
        };
        let Some((fd, proto, published)) = self.sockets.get_mut(&l.number) else {
            return answer(EBADF);
        };
        if *proto == TCP {
            let backlog = libc::c_int::try_from(l.backlog).unwrap_or(libc::c_int::MAX);
            // SAFETY: listen(2) of a socket of ours.
            if unsafe { libc::listen(fd.as_raw_fd(), backlog) } != 0 {
                return answer(linux_errno(errno()));
            }
        }
        if *published {
            return answer(0);
        }
        let mut payload = l.number.to_be_bytes().to_vec();
        payload.extend_from_slice(&l.guest_port.to_be_bytes());
        payload.push(*proto);
        match shards_ipc::send(
            &self.net,
            shards_ipc::kind::NET_PUBLISHED,
            &payload,
            &[fd.as_fd()],
        ) {
            Ok(()) => {
                *published = true;
                answer(0)
            }
            Err(_) => answer(ENETDOWN),
        }
    }

    fn unbind(&mut self, number: u32) {
        if let Some((_, _, true)) = self.sockets.remove(&number)
            && shards_ipc::send(
                &self.net,
                shards_ipc::kind::NET_UNPUBLISHED,
                &number.to_be_bytes(),
                &[],
            )
            .is_ok()
        {
            self.owed += 1;
        }
    }
}

/// Whether `sock` has something to read within `wait`.
fn readable(sock: &UnixStream, wait: Duration) -> bool {
    let mut p = libc::pollfd {
        fd: sock.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = libc::c_int::try_from(wait.as_millis().saturating_add(1)).unwrap_or(libc::c_int::MAX);
    // SAFETY: poll(2) of one pollfd of ours.
    unsafe { libc::poll(&raw mut p, 1, ms) > 0 }
}

fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// A socket of the host's for `b`, bound, and its port: of its protocol, at its address,
/// with the guest's socket's options (IPv6 alone or not set either way, as each host's
/// default differs), close-on-exec, and a UDP socket's datagrams' destinations said
/// (`IP_PKTINFO`), as the network process reads them; Linux's errno where it cannot be.
fn socket(b: &HostBind) -> Result<(OwnedFd, u16), i32> {
    let failed = |e: std::io::Error| linux_errno(e.raw_os_error().unwrap_or(libc::EIO));
    let (family, kind) = (
        if b.addr.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        },
        match b.proto {
            TCP => libc::SOCK_STREAM,
            UDP => libc::SOCK_DGRAM,
            _ => return Err(linux_errno(libc::EPROTONOSUPPORT)),
        },
    );
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let kind = kind | libc::SOCK_CLOEXEC;
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(family, kind, 0) };
    if fd < 0 {
        return Err(linux_errno(errno()));
    }
    // SAFETY: a descriptor just made, ours alone.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let raw = fd.as_raw_fd();
    let set = |level, name, on: bool| {
        let v = libc::c_int::from(on);
        // SAFETY: setsockopt(2) on our descriptor, with a c_int of its length.
        unsafe {
            libc::setsockopt(
                raw,
                level,
                name,
                (&raw const v).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: fcntl(2) on our descriptor.
    unsafe {
        libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    if b.reuse_addr {
        set(libc::SOL_SOCKET, libc::SO_REUSEADDR, true);
    }
    if b.reuse_port {
        set(libc::SOL_SOCKET, libc::SO_REUSEPORT, true);
    }
    if b.addr.is_ipv6() {
        set(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, b.v6only);
    }
    if b.proto == UDP {
        shards_net::pktinfo::enable(fd.as_fd(), b.addr.is_ipv6()).map_err(failed)?;
    }
    let (storage, len) = shards_net::pktinfo::sockaddr_of(SocketAddr::new(b.addr, b.port));
    // SAFETY: a sockaddr of its own length.
    if unsafe { libc::bind(raw, (&raw const storage).cast(), len) } != 0 {
        return Err(linux_errno(errno()));
    }
    let port = bound_port(&fd)?;
    Ok((fd, port))
}

/// The port a socket is bound to: the host's choice, for port 0.
fn bound_port(fd: &OwnedFd) -> Result<u16, i32> {
    // SAFETY: zeroed is a valid sockaddr_storage.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: getsockname(2) into storage of ours, its length given.
    if unsafe { libc::getsockname(fd.as_raw_fd(), (&raw mut ss).cast(), &raw mut len) } != 0 {
        return Err(linux_errno(errno()));
    }
    match i32::from(ss.ss_family) {
        libc::AF_INET => {
            // SAFETY: storage of the family AF_INET holds a sockaddr_in.
            let sin = unsafe { &*(&raw const ss).cast::<libc::sockaddr_in>() };
            Ok(u16::from_be(sin.sin_port))
        }
        libc::AF_INET6 => {
            // SAFETY: storage of the family AF_INET6 holds a sockaddr_in6.
            let sin6 = unsafe { &*(&raw const ss).cast::<libc::sockaddr_in6>() };
            Ok(u16::from_be(sin6.sin6_port))
        }
        _ => Err(linux_errno(libc::EAFNOSUPPORT)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// A guest's bind on the host's network (D124), as the host makes it: the port the
    /// host chose, a port the host holds refused as Linux says it, a listen given to the
    /// network process with the guest's port, and the port let go as the guest's socket
    /// goes.
    #[test]
    fn a_guests_bind_is_the_hosts() {
        let (daemon, net) = UnixStream::pair().unwrap();
        let mut h = HostNet::new(daemon, Duration::from_secs(1));
        let ask = |request, port| HostBind {
            request,
            proto: TCP,
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            reuse_addr: false,
            reuse_port: false,
            v6only: false,
        };
        let frame = |which: u8, payload: &[u8]| [&[which][..], payload].concat();
        let bound = HostBound::decode(&h.ask(&frame(kind::BIND, &ask(1, 0).encode())).unwrap()).unwrap();
        assert_eq!((bound.request, bound.errno), (1, 0));
        assert_ne!(bound.port, 0, "the host's choice");
        let taken =
            HostBound::decode(&h.ask(&frame(kind::BIND, &ask(2, bound.port).encode())).unwrap()).unwrap();
        assert_eq!((taken.request, taken.errno), (2, 98), "EADDRINUSE, Linux's");
        let listen = HostListen {
            request: 3,
            number: bound.number,
            backlog: 16,
            guest_port: 8080,
        };
        let listened = HostBound::decode(&h.ask(&frame(kind::LISTEN, &listen.encode())).unwrap()).unwrap();
        assert_eq!((listened.request, listened.errno), (3, 0));
        let m = shards_ipc::recv(&net).unwrap().unwrap();
        assert_eq!(m.kind, shards_ipc::kind::NET_PUBLISHED);
        let mut said = bound.number.to_be_bytes().to_vec();
        said.extend_from_slice(&8080u16.to_be_bytes());
        said.push(TCP);
        assert_eq!((m.payload, m.fds.len()), (said, 1));
        let at = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bound.port);
        assert!(std::net::TcpStream::connect(at).is_ok(), "listening");
        // The network process's copy held until it answers: a bind of the port waits for
        // that, rather than finding it held.
        let fd = m.fds;
        assert!(h.ask(&frame(kind::UNBIND, &bound.number.to_be_bytes())).is_none());
        let m = shards_ipc::recv(&net).unwrap().unwrap();
        assert_eq!(
            (m.kind, m.payload),
            (
                shards_ipc::kind::NET_UNPUBLISHED,
                bound.number.to_be_bytes().to_vec()
            )
        );
        let answered = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(fd);
            shards_ipc::send(&net, shards_ipc::kind::NET_UNPUBLISHED, &[], &[]).unwrap();
            net
        });
        let again =
            HostBound::decode(&h.ask(&frame(kind::BIND, &ask(4, bound.port).encode())).unwrap()).unwrap();
        assert_eq!(
            (again.request, again.errno, again.port),
            (4, 0, bound.port),
            "its port the guest's again"
        );
        let net = answered.join().unwrap();
        assert!(h.ask(&frame(kind::UNBIND, &again.number.to_be_bytes())).is_none());
        drop(net);
        let unknown = HostListen { number: 99, ..listen };
        let unknown = HostBound::decode(&h.ask(&frame(kind::LISTEN, &unknown.encode())).unwrap()).unwrap();
        assert_eq!(unknown.errno, EBADF);
    }
}
