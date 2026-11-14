//! The agents' resolver (D59): a process of init's in the switch's network namespace,
//! answering on every switch address, so that each agent asks its own gateway. Networks are
//! default deny, and a question is itself a flow past the microVM (its name reaches the
//! resolvers upstream), so an agent may ask only what a grant names: any name where its
//! network says `--dns`, else its remote MCP servers' own. It knows who asks by the link
//! the question arrives on (IP_PKTINFO), which no agent chooses, and answers from the
//! address asked. What it may ask goes to the microVM's
//! resolver, the network process at eth0's gateway, which holds the microVM to the union
//! of its agents' grants; the rest is REFUSED at once. Queries wait on no other query: each
//! is sent upstream under an ID of the relay's, and answered as its answer comes.
//!
//! It answers over TCP too, for answers too long for UDP (RFC 7766): a resolver asks again
//! over TCP when a UDP answer comes truncated. A TCP asker is known by its address, which
//! the switch's strict reverse-path filter holds to its link and which no agent can use
//! without the answers coming to it (a connection is made only where its answers arrive).
//! Each connection is relayed over a TCP connection of its own to the microVM's resolver;
//! each asker may hold an equal share of the descriptors the relay has to spare.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::dnsrelay::Flight;

/// Init's end of the relay's control link, by which askers come and go as the members a
/// microVM's engine makes join and leave (D181): each message an asker to keep (`A`, its
/// link's index, whether it may ask any name, how many addresses it has and each,
/// big-endian, then each table of names it is answered here, its length a big-endian u16
/// first) or one to let go (`R`, its link's index).
static CONTROL: OnceLock<OwnedFd> = OnceLock::new();

/// How long a query waits on the microVM's resolver: an asker's resolver asks again after
/// its own 5 s (RES_TIMEOUT, glibc's and musl's), so one unanswered by then is let go.
const WAIT: Duration = Duration::from_secs(5);

/// What an agent may ask, by the address it asks from.
#[derive(Debug, Clone)]
pub struct Asker {
    /// The switch's end of its link (`d<n>`), by interface index: who asks is known by the
    /// link a query arrives on, never by the address it says it is from (§9.7).
    pub link: u32,
    /// Any name (`NETWORK --dns`).
    pub any: bool,
    /// Else these alone, lowered: its remote MCP servers' hosts.
    pub names: Vec<String>,
    /// Its link's addresses, which a TCP question comes from.
    pub addrs: Vec<Ipv4Addr>,
    /// The names answered it here, never asked upstream: a member's peers it may reach on
    /// its engine's networks, a table a network (D181).
    pub local: Vec<shards_dns::Names>,
}

impl Asker {
    /// Whether it may ask the question `q`.
    fn may_ask(&self, q: &[u8]) -> bool {
        self.any || query_name(q).is_some_and(|name| self.names.contains(&name))
    }

    /// What question `q` of its is answered, as its grants stand.
    fn verdict(&self, q: &[u8]) -> Verdict {
        if let Some(answer) = self.local.iter().find(|t| t.holds(q)).and_then(|t| t.answer(q)) {
            Verdict::Here(answer)
        } else if self.may_ask(q) {
            Verdict::Upstream
        } else {
            Verdict::Refused
        }
    }
}

/// What a question is answered: from its asker's names here (D181), by the microVM's
/// resolver, or REFUSED.
enum Verdict {
    Here(Vec<u8>),
    Upstream,
    Refused,
}

/// Starts the relay on `listen` (port 53 of every switch address) and `upstream`
/// (connected to the microVM's resolver), both of the switch's namespace, in a process of
/// its own.
/// The relay's sockets, of the switch's namespace (`links::Switch::resolver_sockets`).
pub struct Sockets {
    /// Port 53 of every switch address, by UDP and by TCP.
    pub listen: UdpSocket,
    pub tcp: TcpListener,
    /// Connected to the microVM's resolver.
    pub upstream: UdpSocket,
    /// The switch's namespace, which the relay joins, so that its connections upstream
    /// leave the way its UDP questions do.
    pub ns: std::os::fd::OwnedFd,
}

pub fn start(sockets: Sockets, askers: Vec<Asker>) -> Result<(), String> {
    let Sockets {
        listen,
        tcp,
        upstream,
        ns,
    } = sockets;
    let (ours, theirs) =
        crate::hostbind::seqpacket().map_err(|e| format!("the agents' resolver's control link: {e}"))?;
    for s in [&listen, &upstream] {
        s.set_nonblocking(true)
            .map_err(|e| format!("the agents' resolver: {e}"))?;
    }
    // Each question's arrival: the link it came on, and the address asked (ip(7)).
    let on: libc::c_int = 1;
    // SAFETY: setsockopt(2) of an int option on our own socket.
    if unsafe {
        libc::setsockopt(
            listen.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_PKTINFO,
            (&raw const on).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    } != 0
    {
        return Err(format!(
            "the agents' resolver: IP_PKTINFO: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: fork(2) from init with no other thread running (the namespaces' threads are
    // joined): the child runs only this module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(format!(
            "forking the agents' resolver: {}",
            io::Error::last_os_error()
        )),
        0 => {
            crate::dnsrelay::keep_only(&[
                listen.as_raw_fd(),
                upstream.as_raw_fd(),
                tcp.as_raw_fd(),
                ns.as_raw_fd(),
                theirs.as_raw_fd(),
            ]);
            // SAFETY: setns(2) of the switch's namespace, a descriptor this process owns,
            // before it has another thread; closed after.
            let joined = unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) } == 0;
            drop(ns);
            let askers = RwLock::new(askers);
            std::thread::scope(|scope| {
                if joined && let Ok(to) = upstream.peer_addr() {
                    let (tcp, askers) = (&tcp, &askers);
                    // A thread of the relay's own: what it cannot start leaves TCP
                    // unanswered, and UDP as it was.
                    let _ = std::thread::Builder::new()
                        .name("agentdns-tcp".into())
                        .spawn_scoped(scope, move || serve_tcp(tcp, to, askers));
                }
                relay(&listen, &upstream, &askers, &theirs);
            });
            // SAFETY: _exit(2) of the child, which owns nothing to flush.
            unsafe { libc::_exit(0) }
        }
        _ => {
            let _ = CONTROL.set(ours);
            Ok(())
        }
    }
}

/// Whether the relay runs: started for the domains, or for the first member a microVM's
/// engine made that asks (D181).
pub fn running() -> bool {
    CONTROL.get().is_some()
}

/// Has the relay keep `asker`, in place of any of its link's (D181).
pub fn admit(asker: &Asker) -> io::Result<()> {
    let mut m = vec![b'A'];
    m.extend_from_slice(&asker.link.to_be_bytes());
    m.push(u8::from(asker.any));
    m.push(
        u8::try_from(asker.addrs.len()).map_err(|_| io::Error::other("an asker with too many addresses"))?,
    );
    for a in &asker.addrs {
        m.extend_from_slice(&a.octets());
    }
    for table in &asker.local {
        let bytes = table.table();
        let len =
            u16::try_from(bytes.len()).map_err(|_| io::Error::other("a table of names too long to say"))?;
        m.extend_from_slice(&len.to_be_bytes());
        m.extend_from_slice(&bytes);
    }
    tell(&m)
}

/// Has the relay let the asker of link `link` go (D181).
pub fn release(link: u32) -> io::Result<()> {
    let mut m = vec![b'R'];
    m.extend_from_slice(&link.to_be_bytes());
    tell(&m)
}

fn tell(m: &[u8]) -> io::Result<()> {
    let sock = CONTROL
        .get()
        .ok_or_else(|| io::Error::other("the agents' resolver is not running"))?;
    // SAFETY: send(2) of our own bytes on our own descriptor.
    let sent = unsafe { libc::send(sock.as_raw_fd(), m.as_ptr().cast(), m.len(), libc::MSG_NOSIGNAL) };
    if usize::try_from(sent).ok() == Some(m.len()) {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What a control message says of `askers`, done to them; false where it says nothing
/// this relay knows.
fn apply(m: &[u8], askers: &RwLock<Vec<Asker>>) -> bool {
    let link = |m: &[u8]| {
        m.get(1..5)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_be_bytes)
    };
    match m.first() {
        Some(b'A') => {
            let (Some(link), Some(&any), Some(&count)) = (link(m), m.get(5), m.get(6)) else {
                return false;
            };
            let end = 7 + 4 * usize::from(count);
            let Some((addrs, [])) = m.get(7..end).map(<[u8]>::as_chunks::<4>) else {
                return false;
            };
            let addrs = addrs.iter().map(|c| Ipv4Addr::from(*c)).collect();
            let mut local = Vec::new();
            let mut rest = m.get(end..).unwrap_or_default();
            while let Some((&[a, b], tail)) = rest.split_first_chunk::<2>() {
                let len = usize::from(u16::from_be_bytes([a, b]));
                let Some((table, tail)) = tail.split_at_checked(len) else {
                    return false;
                };
                let Some(names) = shards_dns::Names::decode(table) else {
                    return false;
                };
                local.push(names);
                rest = tail;
            }
            if !rest.is_empty() {
                return false;
            }
            let mut a = askers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            a.retain(|x| x.link != link);
            a.push(Asker {
                link,
                any: any != 0,
                names: Vec::new(),
                addrs,
                local,
            });
            true
        }
        Some(b'R') => {
            let Some(link) = link(m) else { return false };
            askers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|x| x.link != link);
            true
        }
        _ => false,
    }
}

/// A query's one question's name, lowered (RFC 1035 §4.1.2); None for anything else.
fn query_name(q: &[u8]) -> Option<String> {
    if q.get(2)? & 0x80 != 0 || u16::from_be_bytes([*q.get(4)?, *q.get(5)?]) != 1 {
        return None;
    }
    let mut labels = Vec::new();
    let mut at = 12;
    loop {
        let len = usize::from(*q.get(at)?);
        if len == 0 {
            break;
        }
        if len > 63 || at > 12 + 255 {
            return None;
        }
        labels.push(String::from_utf8_lossy(q.get(at + 1..at + 1 + len)?).to_ascii_lowercase());
        at += 1 + len;
    }
    Some(labels.join("."))
}

/// REFUSED (RCODE 5) for query `q`, its header's ID and question kept.
fn refused(q: &[u8]) -> Option<Vec<u8>> {
    let mut out = q.to_vec();
    *out.get_mut(2)? |= 0x80;
    *out.get_mut(3)? = (*out.get(3)? & 0xf0) | 5;
    // Its question alone: no answer, authority or additional records.
    for i in 6..12 {
        *out.get_mut(i)? = 0;
    }
    Some(out)
}

/// Where a question came from: its asker's address and port, the link it arrived on, and
/// the address it was asked of, which its answer comes from.
#[derive(Debug, Clone, Copy)]
struct Arrival {
    from: libc::sockaddr_in,
    link: u32,
    asked: libc::in_addr,
}

/// A datagram on `sock`, with its arrival (`IP_PKTINFO`, ip(7)).
fn receive(sock: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, Arrival)> {
    // SAFETY: all-zero is valid for each of these out-parameters.
    let mut from: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: as above.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = (&raw mut from).cast();
    msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: recvmsg(2) into the buffers just described, all of ours and alive.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, 0) };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    // SAFETY: CMSG_FIRSTHDR and CMSG_NXTHDR walk the control buffer recvmsg filled, and
    // CMSG_DATA of an IP_PKTINFO header points at an in_pktinfo (ip(7)).
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&raw const msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::IPPROTO_IP && (*c).cmsg_type == libc::IP_PKTINFO {
                let info = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<libc::in_pktinfo>());
                return Ok((
                    n,
                    Arrival {
                        from,
                        link: u32::try_from(info.ipi_ifindex).unwrap_or(0),
                        asked: info.ipi_spec_dst,
                    },
                ));
            }
            c = libc::CMSG_NXTHDR(&raw const msg, c);
        }
    }
    Err(io::Error::other("a question with no arrival"))
}

/// Sends `answer` back as `to` came: to its asker, from the address it asked, on its link.
fn answer_to(sock: &UdpSocket, answer: &[u8], to: &Arrival) -> io::Result<()> {
    let info = libc::in_pktinfo {
        ipi_ifindex: i32::try_from(to.link).unwrap_or(0),
        ipi_spec_dst: to.asked,
        ipi_addr: libc::in_addr { s_addr: 0 },
    };
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: answer.as_ptr().cast_mut().cast(),
        iov_len: answer.len(),
    };
    let mut dest = to.from;
    // SAFETY: an all-zero msghdr, then filled with buffers of ours that outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = (&raw mut dest).cast();
    msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE and CMSG_LEN of a constant size; CMSG_FIRSTHDR of a control buffer
    // large enough for one in_pktinfo, whose header and data are written in it.
    unsafe {
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::in_pktinfo>() as u32) as _;
        let c = libc::CMSG_FIRSTHDR(&raw const msg);
        if c.is_null() {
            return Err(io::Error::other("no room for the answer's arrival"));
        }
        (*c).cmsg_level = libc::IPPROTO_IP;
        (*c).cmsg_type = libc::IP_PKTINFO;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::in_pktinfo>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::in_pktinfo>(), info);
    }
    // SAFETY: sendmsg(2) of the message just built.
    if unsafe { libc::sendmsg(sock.as_raw_fd(), &raw const msg, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The descriptors each TCP asker may hold: an equal share of those the process may open
/// (RLIMIT_NOFILE) beyond those it has open, divided by the sockets a connection owns.
pub(crate) fn tcp_share(askers: usize, sockets: usize) -> usize {
    // SAFETY: an all-zero rlimit, filled by getrlimit(2).
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: getrlimit(2) into the struct just made.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } == 0 {
        usize::try_from(lim.rlim_cur).unwrap_or(usize::MAX)
    } else {
        0
    };
    let Ok(open) = std::fs::read_dir("/proc/self/fd") else {
        return 0;
    };
    soft.saturating_sub(open.count())
        .checked_div(sockets)
        .unwrap_or(0)
        / askers.max(1)
}

/// The TCP questions: each connection on a thread of its own, its asker known by its
/// address, each question answered from its names here, REFUSED, or relayed to the
/// microVM's resolver `to` over a connection of its own; idle for `WAIT`, it is closed.
fn serve_tcp(listener: &TcpListener, to: SocketAddr, askers: &RwLock<Vec<Asker>>) {
    // By each asker's link, which outlasts its place in the list as askers come and go.
    let held: std::sync::Mutex<HashMap<u32, usize>> = std::sync::Mutex::new(HashMap::new());
    std::thread::scope(|scope| {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let Ok(SocketAddr::V4(from)) = conn.peer_addr() else {
                continue;
            };
            let (link, share) = {
                let a = askers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    a.iter().find(|a| a.addrs.contains(from.ip())).map(|a| a.link),
                    tcp_share(a.len(), 2),
                )
            };
            let Some(link) = link else { continue };
            let asker = Who {
                link,
                addr: *from.ip(),
            };
            {
                let mut h = held.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let n = h.entry(link).or_insert(0);
                if *n >= share {
                    continue;
                }
                *n += 1;
            }
            let held = &held;
            let served = std::thread::Builder::new()
                .name("agentdns-conn".into())
                .spawn_scoped(scope, move || {
                    let _ = serve_connection(&mut conn, to, askers, asker);
                    if let Some(n) = held
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(&link)
                    {
                        *n = n.saturating_sub(1);
                    }
                });
            if served.is_err()
                && let Some(n) = held
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&link)
            {
                *n = n.saturating_sub(1);
            }
        }
    });
}

/// A TCP connection's asker: its link, and the address it connected from.
#[derive(Clone, Copy)]
struct Who {
    link: u32,
    addr: Ipv4Addr,
}

/// Each question on `conn`, judged by its asker's grants as they stand when it comes, not
/// as they stood when the connection opened (network-09): a member's peers that came or
/// went since are answered as they are now (D181), and once its asker is let go of, or its
/// link no longer has the address it connected from, the connection is closed.
fn serve_connection(
    conn: &mut TcpStream,
    to: SocketAddr,
    askers: &RwLock<Vec<Asker>>,
    who: Who,
) -> io::Result<()> {
    conn.set_read_timeout(Some(WAIT))?;
    conn.set_write_timeout(Some(WAIT))?;
    let mut upstream: Option<TcpStream> = None;
    while let Some(q) = crate::dnsrelay::read_message(conn)? {
        let verdict = askers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|a| a.link == who.link && a.addrs.contains(&who.addr))
            .map(|a| a.verdict(&q));
        match verdict {
            None => break,
            Some(Verdict::Here(answer)) => {
                crate::dnsrelay::write_message(conn, &answer)?;
                continue;
            }
            Some(Verdict::Refused) => {
                if let Some(no) = refused(&q) {
                    crate::dnsrelay::write_message(conn, &no)?;
                }
                continue;
            }
            Some(Verdict::Upstream) => {}
        }
        if upstream.is_none() {
            let up = TcpStream::connect_timeout(&to, WAIT)?;
            up.set_read_timeout(Some(WAIT))?;
            up.set_write_timeout(Some(WAIT))?;
            upstream = Some(up);
        }
        let Some(up) = upstream.as_mut() else { break };
        crate::dnsrelay::write_message(up, &q)?;
        match crate::dnsrelay::read_message(up)? {
            Some(answer) => crate::dnsrelay::write_message(conn, &answer)?,
            None => break,
        }
    }
    Ok(())
}

fn relay(listen: &UdpSocket, upstream: &UdpSocket, askers: &RwLock<Vec<Asker>>, control: &OwnedFd) {
    let count = || {
        askers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    };
    let mut flight: Flight<(Arrival, [u8; 2])> = Flight::new(count());
    let mut next: u16 = 0;
    let mut buf = [0u8; 4096];
    let mut said = true;
    loop {
        let mut set = [
            libc::pollfd {
                fd: listen.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: upstream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if said { control.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll(2) of three pollfds of ours, until a question expires or I/O arrives.
        if unsafe { libc::poll(set.as_mut_ptr(), 3, flight.wait_ms(Instant::now())) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return;
        }
        // Askers that came or went, each message whole (SOCK_SEQPACKET); init's end closed,
        // the askers stay as they are.
        if set[2].revents != 0 {
            // A message whole, whatever its tables of names hold (SOCK_SEQPACKET's at most
            // its socket's buffer).
            let mut m = vec![0u8; 1 << 16];
            // SAFETY: recv(2) into our own buffer, without waiting.
            let n = unsafe {
                libc::recv(
                    control.as_raw_fd(),
                    m.as_mut_ptr().cast(),
                    m.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            match usize::try_from(n) {
                Ok(0) => said = false,
                Ok(n) => {
                    if apply(m.get(..n).unwrap_or_default(), askers) {
                        flight.resize(count());
                    }
                }
                Err(_) => {}
            }
        }
        let now = Instant::now();
        flight.expire(now);
        while let Ok((n, from)) = receive(listen, &mut buf) {
            let q = buf.get(..n).unwrap_or_default();
            // Its peers' names, answered here and never asked past the microVM (D181).
            let here = askers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|a| a.link == from.link)
                .and_then(|a| a.local.iter().find(|t| t.holds(q)).and_then(|t| t.answer(q)));
            if let Some(answer) = here {
                let _ = answer_to(listen, &answer, &from);
                continue;
            }
            let granted = askers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|a| a.link == from.link)
                .is_some_and(|a| a.may_ask(q));
            if !granted || !flight.admits(from.link) {
                if let Some(no) = refused(q) {
                    let _ = answer_to(listen, &no, &from);
                }
                continue;
            }
            let Some(&[i0, i1]) = q.first_chunk::<2>() else {
                continue;
            };
            // An ID of the relay's own, not one in flight.
            let Some(id) = flight.vacant_id(next) else {
                continue;
            };
            next = id;
            let mut ours = q.to_vec();
            if let Some(id) = ours.get_mut(..2) {
                id.copy_from_slice(&next.to_be_bytes());
            }
            let link = from.link;
            if flight.insert(next, link, (from, [i0, i1]), now).is_ok() && upstream.send(&ours).is_err() {
                flight.remove(next);
            }
            next = next.wrapping_add(1);
        }
        while let Ok(n) = upstream.recv(&mut buf) {
            let answer = buf.get_mut(..n).unwrap_or_default();
            let Some(&[a, b]) = answer.first_chunk::<2>() else {
                continue;
            };
            let Some((to, id)) = flight.remove(u16::from_be_bytes([a, b])) else {
                continue;
            };
            if let Some(head) = answer.get_mut(..2) {
                head.copy_from_slice(&id);
            }
            let _ = answer_to(listen, answer, &to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_tcp_session_uses_the_members_current_names() {
        fn table(entries: &[(&str, &str)]) -> shards_dns::Names {
            let mut names = shards_dns::Names::default();
            for &(name, ip) in entries {
                names.add(name, ip.parse().unwrap());
            }
            names
        }
        fn question(name: &str) -> Vec<u8> {
            let mut q = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
            q.push(u8::try_from(name.len()).unwrap());
            q.extend_from_slice(name.as_bytes());
            q.extend_from_slice(&[0, 0, 1, 0, 1]);
            q
        }
        fn ask(conn: &mut TcpStream, name: &str) -> Vec<u8> {
            crate::dnsrelay::write_message(conn, &question(name)).unwrap();
            crate::dnsrelay::read_message(conn).unwrap().unwrap()
        }
        let askers = std::sync::Arc::new(RwLock::new(vec![Asker {
            link: 7,
            any: false,
            names: Vec::new(),
            addrs: vec![Ipv4Addr::LOCALHOST],
            local: vec![table(&[("departed", "192.0.2.7"), ("keeper", "192.0.2.8")])],
        }]));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = listener.local_addr().unwrap();
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        upstream.set_nonblocking(true).unwrap();
        let to = upstream.local_addr().unwrap();
        let current = std::sync::Arc::clone(&askers);
        // The filtered cargo test process owns this listener thread and its workers.
        std::thread::spawn(move || serve_tcp(&listener, to, &current));
        let mut conn = TcpStream::connect(at).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(ask(&mut conn, "departed")[3] & 0x0f, 0);
        askers.write().unwrap().first_mut().unwrap().local =
            vec![table(&[("keeper", "192.0.2.8"), ("joined", "192.0.2.9")])];
        assert_eq!(
            ask(&mut conn, "departed")[3] & 0x0f,
            5,
            "the old TCP session kept a departed member"
        );
        let joined = ask(&mut conn, "joined");
        assert_eq!(
            shards_dns::a_records(&joined).unwrap().addresses,
            ["192.0.2.9".parse::<std::net::IpAddr>().unwrap()]
        );
        assert_eq!(ask(&mut conn, "keeper")[3] & 0x0f, 0);
        askers.write().unwrap().clear();
        crate::dnsrelay::write_message(&mut conn, &question("keeper")).unwrap();
        assert!(
            crate::dnsrelay::read_message(&mut conn).unwrap().is_none(),
            "a revoked asker retained its session"
        );
        assert!(
            upstream.accept().is_err(),
            "a local or refused name went upstream"
        );
    }

    /// One asker holding its share of the IDs is refused more, and another still asks; one
    /// answered or let go has room again.
    #[test]
    fn no_asker_takes_anothers_share_of_the_ids() {
        let now = Instant::now();
        let mut f: Flight<()> = Flight::new(2);
        for id in 0..32768u16 {
            assert!(f.admits(7));
            f.insert(id, 7, (), now).unwrap();
        }
        assert!(!f.admits(7));
        assert!(f.admits(9));
        f.insert(40000, 9, (), now).unwrap();
        assert!(f.remove(0).is_some());
        assert!(f.admits(7));
        f.insert(0, 7, (), now).unwrap();
        f.expire(now + WAIT);
        assert!(f.admits(7) && f.admits(9));
        assert_eq!(f.wait_ms(now + WAIT), -1);
    }

    #[test]
    fn a_refused_query_keeps_its_id_and_question() {
        let mut q = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 1];
        q.extend_from_slice(b"\x03Api\x07example\x00\x00\x01\x00\x01");
        assert_eq!(query_name(&q).as_deref(), Some("api.example"));
        let no = refused(&q).unwrap();
        assert_eq!((no[0], no[1], no[2] & 0x80, no[3] & 0x0f), (0xab, 0xcd, 0x80, 5));
        assert_eq!(&no[6..12], &[0; 6]);
        let mut answer = q.clone();
        answer[2] |= 0x80;
        assert_eq!(query_name(&answer), None);
    }
}
