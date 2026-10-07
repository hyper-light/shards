//! Docker's embedded DNS address in the guest (D46): a microVM on a user-defined network
//! asks 127.0.0.11:53, as its /etc/resolv.conf says and as programs written for Docker ask
//! it by address (nginx's `resolver 127.0.0.11`). Docker reaches its resolver there through
//! a NAT rule in the container's namespace (moby libnetwork resolver_unix.go); shards'
//! answers at the network's gateway, in the VM's network process, and a process of init's,
//! outside the workload's cgroup, relays each query there, by UDP and by TCP, and its
//! answer back.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

/// Docker's embedded DNS address (libnetwork resolver.go).
const ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 11);

/// How long a query waits on the gateway's answer: the network process answers its own
/// names at once, and refuses the rest at once, so one that takes this long is lost; the
/// asker's resolver asks again (glibc's and musl's resolvers wait 5 s, RES_TIMEOUT).
const WAIT: Duration = Duration::from_secs(5);

/// Starts the relay to `gateway`'s resolver, in a process of its own.
pub fn start(gateway: Ipv4Addr) -> Result<(), String> {
    // Bound before the fork, so that a failure is init's to report.
    let udp = UdpSocket::bind((ADDRESS, 53)).map_err(|e| format!("binding 127.0.0.11:53/udp: {e}"))?;
    let tcp = TcpListener::bind((ADDRESS, 53)).map_err(|e| format!("binding 127.0.0.11:53/tcp: {e}"))?;
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(format!("forking the DNS relay: {}", io::Error::last_os_error())),
        0 => {
            keep_only(&[
                std::os::fd::AsRawFd::as_raw_fd(&udp),
                std::os::fd::AsRawFd::as_raw_fd(&tcp),
            ]);
            relay(&udp, &tcp, SocketAddr::from((gateway, 53)));
            // SAFETY: _exit(2) of the child, which owns nothing to flush.
            unsafe { libc::_exit(0) }
        }
        _ => Ok(()),
    }
}

/// Closes every descriptor but `keep` and the standard three: none of init's
/// connections stays open in the relay.
pub(crate) fn keep_only(keep: &[i32]) {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return;
    };
    let fds: Vec<i32> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
        .filter(|fd| *fd > 2 && !keep.contains(fd))
        .collect();
    for fd in fds {
        // SAFETY: close(2) of a descriptor this process holds and will not use.
        unsafe { libc::close(fd) };
    }
}

/// Relays queries until the VM ends: UDP's as they come, and TCP's a connection at a time.
fn relay(udp: &UdpSocket, tcp: &TcpListener, gateway: SocketAddr) {
    let fds = [
        std::os::fd::AsRawFd::as_raw_fd(udp),
        std::os::fd::AsRawFd::as_raw_fd(tcp),
    ];
    let mut buf = [0u8; 65535];
    loop {
        let mut polled = fds.map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: poll(2) on two descriptors of ours, waiting for ever.
        if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if polled[0].revents != 0
            && let Ok((n, from)) = udp.recv_from(&mut buf)
            && let Some(answer) = ask(gateway, buf.get(..n).unwrap_or_default())
        {
            let _ = udp.send_to(&answer, from);
        }
        if polled[1].revents != 0
            && let Ok((conn, _)) = tcp.accept()
        {
            let _ = serve_tcp(conn, gateway);
        }
    }
}

/// The gateway's answer to `query`, by UDP.
fn ask(gateway: SocketAddr, query: &[u8]) -> Option<Vec<u8>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect(gateway).ok()?;
    sock.set_read_timeout(Some(WAIT)).ok()?;
    sock.send(query).ok()?;
    let mut buf = vec![0u8; 65535];
    let n = sock.recv(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// One TCP connection's queries (RFC 1035 §4.2.2: each its length's two bytes first),
/// each asked of the gateway by UDP, until it closes.
fn serve_tcp(mut conn: TcpStream, gateway: SocketAddr) -> io::Result<()> {
    conn.set_read_timeout(Some(WAIT))?;
    loop {
        let mut len = [0u8; 2];
        conn.read_exact(&mut len)?;
        let mut query = vec![0u8; usize::from(u16::from_be_bytes(len))];
        conn.read_exact(&mut query)?;
        let Some(answer) = ask(gateway, &query) else {
            return Ok(());
        };
        let len = u16::try_from(answer.len()).map_err(|_| io::Error::other("an answer too long"))?;
        conn.write_all(&len.to_be_bytes())?;
        conn.write_all(&answer)?;
    }
}
