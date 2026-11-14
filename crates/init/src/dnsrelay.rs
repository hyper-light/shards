//! A network's names in the guest (D46): a microVM on a shards network asks 127.0.0.11:53,
//! as its /etc/resolv.conf says and as programs written for Docker ask it by address
//! (nginx's `resolver 127.0.0.11`). The network's names are answered at its gateway, in
//! the VM's network process; a process of init's, outside the workload's cgroup, relays
//! each query there, by UDP and by TCP, and its answer back.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The address programs written for Docker ask names of by address.
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
            let _ = relay(&udp, &tcp, SocketAddr::from((gateway, 53)));
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

/// Questions owned by their relay ID until answered or expired. Both guest resolvers
/// use this owner; each link has an equal share of the complete 16-bit DNS ID space.
pub(crate) struct Flight<T> {
    pending: HashMap<u16, (u32, T, Instant)>,
    held: HashMap<u32, usize>,
    share: usize,
}

impl<T> Flight<T> {
    pub(crate) fn new(askers: usize) -> Self {
        let mut flight = Self {
            pending: HashMap::new(),
            held: HashMap::new(),
            share: 0,
        };
        flight.resize(askers);
        flight
    }

    pub(crate) fn resize(&mut self, askers: usize) {
        self.share = (usize::from(u16::MAX) + 1) / askers.max(1);
    }

    pub(crate) fn admits(&self, link: u32) -> bool {
        self.held.get(&link).copied().unwrap_or(0) < self.share
    }

    pub(crate) fn vacant_id(&self, start: u16) -> Option<u16> {
        (0..=u16::MAX)
            .map(|offset| start.wrapping_add(offset))
            .find(|id| !self.pending.contains_key(id))
    }

    pub(crate) fn insert(&mut self, id: u16, link: u32, what: T, now: Instant) -> io::Result<()> {
        if !self.admits(link) {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        if self.pending.contains_key(&id) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let until = now
            .checked_add(WAIT)
            .ok_or_else(|| io::Error::other("DNS deadline exceeds the clock"))?;
        let held = self
            .held
            .get(&link)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("too many DNS questions"))?;
        self.pending.try_reserve(1).map_err(io::Error::other)?;
        self.held.try_reserve(1).map_err(io::Error::other)?;
        self.pending.insert(id, (link, what, until));
        self.held.insert(link, held);
        Ok(())
    }

    pub(crate) fn remove(&mut self, id: u16) -> Option<T> {
        let (link, what, _) = self.pending.remove(&id)?;
        if let Some(n) = self.held.get_mut(&link) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.held.remove(&link);
            }
        }
        Some(what)
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        let held = &mut self.held;
        self.pending.retain(|_, (link, _, until)| {
            if *until > now {
                return true;
            }
            if let Some(n) = held.get_mut(link) {
                *n = n.saturating_sub(1);
            }
            false
        });
        self.held.retain(|_, n| *n > 0);
    }

    /// poll(2)'s millisecond ceiling, or its -1 identity for no deadline. No periodic
    /// probe is needed: either readiness or the earliest outstanding question wakes us.
    pub(crate) fn wait_ms(&self, now: Instant) -> i32 {
        let Some(until) = self.pending.values().map(|(_, _, at)| *at).min() else {
            return -1;
        };
        let left = until.saturating_duration_since(now);
        let ms = left.as_millis();
        let floor = Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX));
        let ceiling = ms.saturating_add(u128::from(left > floor));
        i32::try_from(ceiling).unwrap_or(i32::MAX)
    }
}

// The relay is one process per VM; a permit owns one TCP worker through every exit,
// including a failed spawn. No per-connection allocation is needed for this count.
static TCP_ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct TcpPermit;

impl TcpPermit {
    fn take(capacity: usize) -> Option<Self> {
        TCP_ACTIVE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                if active < capacity {
                    active.checked_add(1)
                } else {
                    None
                }
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for TcpPermit {
    fn drop(&mut self) {
        TCP_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Relays UDP transactions concurrently; a TCP connection owns only its worker.
fn relay(udp: &UdpSocket, tcp: &TcpListener, gateway: SocketAddr) -> io::Result<()> {
    let upstream = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    upstream.connect(gateway)?;
    udp.set_nonblocking(true)?;
    tcp.set_nonblocking(true)?;
    upstream.set_nonblocking(true)?;
    // At most three sockets per TCP worker: client, current UDP ask, and a retained
    // TCP fallback. The available descriptors are observed after fixed sockets open.
    let capacity = crate::agentdns::tcp_share(1, 3);
    let fds = [
        std::os::fd::AsRawFd::as_raw_fd(udp),
        std::os::fd::AsRawFd::as_raw_fd(tcp),
        std::os::fd::AsRawFd::as_raw_fd(&upstream),
    ];
    let mut flight: Flight<(SocketAddr, [u8; 2])> = Flight::new(1);
    let mut next = 0;
    let mut buf = [0u8; 65535];
    loop {
        let mut polled = fds.map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: poll(2) on our three descriptors until readiness or query expiry.
        if unsafe { libc::poll(polled.as_mut_ptr(), 3, flight.wait_ms(Instant::now())) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        let now = Instant::now();
        flight.expire(now);
        if polled[0].revents != 0
            && let Ok((n, from)) = udp.recv_from(&mut buf)
            && let Some(query) = buf.get_mut(..n)
            && let Some(id) = query.get_mut(..2)
            && let Ok(original) = <[u8; 2]>::try_from(&*id)
            && flight.admits(0)
            && let Some(ours) = flight.vacant_id(next)
        {
            id.copy_from_slice(&ours.to_be_bytes());
            if flight.insert(ours, 0, (from, original), now).is_ok() && upstream.send(query).is_err() {
                flight.remove(ours);
            }
            next = ours.wrapping_add(1);
        }
        if polled[1].revents != 0
            && let Ok((conn, _)) = tcp.accept()
            && let Some(permit) = TcpPermit::take(capacity)
        {
            // Like agentdns's workers, scoped to this relay process: start's _exit
            // ends them all if its poller fails or the VM ends. Builder failure drops
            // the captured permit and connection too.
            let _ = std::thread::Builder::new()
                .name("dnsrelay-tcp".into())
                .spawn(move || {
                    let _permit = permit;
                    let _ = serve_tcp(conn, gateway);
                });
        }
        if polled[2].revents != 0
            && let Ok(n) = upstream.recv(&mut buf)
            && let Some(answer) = buf.get_mut(..n)
            && let Some(id) = answer.get_mut(..2)
            && let Ok(raw) = <[u8; 2]>::try_from(&*id)
            && let Some((to, original)) = flight.remove(u16::from_be_bytes(raw))
        {
            id.copy_from_slice(&original);
            let _ = udp.send_to(answer, to);
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
/// each asked of the gateway by UDP, which answers the network's names, and again by TCP
/// where its answer came truncated (RFC 7766 §5), until it closes.
fn serve_tcp(mut conn: TcpStream, gateway: SocketAddr) -> io::Result<()> {
    conn.set_read_timeout(Some(WAIT))?;
    conn.set_write_timeout(Some(WAIT))?;
    let mut by_tcp: Option<TcpStream> = None;
    while let Some(query) = read_message(&mut conn)? {
        let Some(mut answer) = ask(gateway, &query) else {
            return Ok(());
        };
        // TC (RFC 1035 §4.1.1): asked again by TCP, the truncated answer kept where that
        // cannot be.
        if answer.get(2).is_some_and(|f| f & 0x02 != 0)
            && let Ok(Some(whole)) = ask_tcp(&mut by_tcp, gateway, &query)
        {
            answer = whole;
        }
        write_message(&mut conn, &answer)?;
    }
    Ok(())
}

/// The gateway's answer to `query` by TCP, on the connection `held`, made where there is
/// none, and dropped where it fails.
fn ask_tcp(held: &mut Option<TcpStream>, gateway: SocketAddr, query: &[u8]) -> io::Result<Option<Vec<u8>>> {
    if held.is_none() {
        let s = TcpStream::connect_timeout(&gateway, WAIT)?;
        s.set_read_timeout(Some(WAIT))?;
        s.set_write_timeout(Some(WAIT))?;
        *held = Some(s);
    }
    let Some(s) = held.as_mut() else { return Ok(None) };
    let answer = write_message(s, query).and_then(|()| read_message(s));
    if !matches!(answer, Ok(Some(_))) {
        *held = None;
    }
    answer
}

/// One length-prefixed DNS message on `s` (RFC 7766 §8), or none at its end.
pub(crate) fn read_message(s: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 2];
    match s.read_exact(&mut len) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        r => r?,
    }
    let mut msg = vec![0u8; usize::from(u16::from_be_bytes(len))];
    s.read_exact(&mut msg)?;
    Ok(Some(msg))
}

pub(crate) fn write_message(s: &mut TcpStream, msg: &[u8]) -> io::Result<()> {
    let len = u16::try_from(msg.len()).map_err(|_| io::Error::other("an answer over 65535 bytes"))?;
    let mut out = Vec::with_capacity(msg.len() + 2);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(msg);
    s.write_all(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_id_space_refuses_then_releases_its_questions() {
        let now = Instant::now();
        let mut flight = Flight::new(1);
        for id in 0..=u16::MAX {
            flight.insert(id, 7, (), now).unwrap();
        }
        assert!(!flight.admits(7));
        assert_eq!(flight.vacant_id(0), None);
        assert!(flight.remove(42).is_some());
        assert_eq!(flight.vacant_id(0), Some(42));
        flight.insert(42, 7, (), now).unwrap();
        flight.expire(now + WAIT);
        assert!(flight.admits(7));
        assert_eq!(flight.vacant_id(0), Some(0));
        assert_eq!(flight.wait_ms(now + WAIT), -1);
    }

    #[test]
    fn an_open_tcp_client_and_a_lost_udp_query_leave_other_clients_progress() {
        let gateway = UdpSocket::bind("127.0.0.1:0").unwrap();
        gateway.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let to = gateway.local_addr().unwrap();
        let mut names = shards_dns::Names::default();
        names.add("peer", "192.0.2.7".parse().unwrap());
        let mut q = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(b"\x04peer\0\0\x01\0\x01");
        let expected = names.answer(&q).unwrap();
        let upstream = std::thread::spawn(move || {
            for _ in 0..6 {
                let mut buf = [0; 65535];
                let (n, from) = gateway.recv_from(&mut buf).unwrap();
                let query = &buf[..n];
                if shards_dns::query_name(query).as_deref() == Some("lost") {
                    continue;
                }
                gateway.send_to(&names.answer(query).unwrap(), from).unwrap();
            }
        });
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let (udp_at, tcp_at) = (udp.local_addr().unwrap(), tcp.local_addr().unwrap());
        // The cargo test process owns this relay thread and all sockets; no VM or
        // external daemon survives that process, including the baseline assertion.
        std::thread::spawn(move || relay(&udp, &tcp, to));
        let mut first = TcpStream::connect(tcp_at).unwrap();
        first.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        write_message(&mut first, &q).unwrap();
        assert_eq!(read_message(&mut first).unwrap(), Some(expected.clone()));

        let lost = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut lost_q = q.clone();
        lost_q[12..18].copy_from_slice(b"\x04lost\0");
        lost.send_to(&lost_q, udp_at).unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").unwrap();
        other.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut buf = [0; 65535];
        other.send_to(&q, udp_at).unwrap();
        let answer = other.recv(&mut buf);
        assert!(
            answer.is_ok(),
            "an open TCP client blocked another client's UDP query: {answer:?}"
        );
        assert_eq!(&buf[..answer.unwrap()], &expected);

        let mut second = TcpStream::connect(tcp_at).unwrap();
        second.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        write_message(&mut second, &q).unwrap();
        assert_eq!(read_message(&mut second).unwrap(), Some(expected.clone()));
        // An incomplete length prefix keeps the first connection open too.
        first.write_all(&[0]).unwrap();
        other.send_to(&q, udp_at).unwrap();
        let n = other.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], &expected);
        write_message(&mut second, &q).unwrap();
        assert_eq!(read_message(&mut second).unwrap(), Some(expected));
        drop((first, second, lost, other));
        upstream.join().unwrap();
    }
}
