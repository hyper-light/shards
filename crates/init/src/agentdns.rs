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

use std::collections::HashMap;
use std::io;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

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
}

/// Starts the relay on `listen` (port 53 of every switch address) and `upstream`
/// (connected to the microVM's resolver), both of the switch's namespace, in a process of
/// its own.
pub fn start(listen: UdpSocket, upstream: UdpSocket, askers: Vec<Asker>) -> Result<(), String> {
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
            crate::dnsrelay::keep_only(&[listen.as_raw_fd(), upstream.as_raw_fd()]);
            relay(&listen, &upstream, &askers);
            // SAFETY: _exit(2) of the child, which owns nothing to flush.
            unsafe { libc::_exit(0) }
        }
        _ => Ok(()),
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

/// The questions in flight, each under an ID of the relay's own, and how many each asker's
/// link holds: each may hold an equal share of the IDs (`u16::MAX` over the askers), so
/// that one asking without end takes no other's.
struct Flight<T> {
    pending: HashMap<u16, (u32, T, Instant)>,
    held: HashMap<u32, usize>,
    share: usize,
}

impl<T> Flight<T> {
    fn new(askers: usize) -> Self {
        Flight {
            pending: HashMap::new(),
            held: HashMap::new(),
            share: usize::from(u16::MAX) / askers.max(1),
        }
    }

    /// Whether a question from `link` may go: it holds less than its share.
    fn admits(&self, link: u32) -> bool {
        self.held.get(&link).copied().unwrap_or(0) < self.share
    }

    fn insert(&mut self, id: u16, link: u32, what: T, at: Instant) {
        if let Some((old, _, _)) = self.pending.insert(id, (link, what, at)) {
            self.release(old);
        }
        *self.held.entry(link).or_insert(0) += 1;
    }

    fn remove(&mut self, id: u16) -> Option<T> {
        let (link, what, _) = self.pending.remove(&id)?;
        self.release(link);
        Some(what)
    }

    /// Lets go what has waited `WAIT`.
    fn expire(&mut self, now: Instant) {
        let gone: Vec<u32> = self
            .pending
            .values()
            .filter(|(_, _, at)| now.duration_since(*at) >= WAIT)
            .map(|(link, _, _)| *link)
            .collect();
        self.pending
            .retain(|_, (_, _, at)| now.duration_since(*at) < WAIT);
        for link in gone {
            self.release(link);
        }
    }

    fn release(&mut self, link: u32) {
        if let Some(n) = self.held.get_mut(&link) {
            *n = n.saturating_sub(1);
        }
    }
}

fn relay(listen: &UdpSocket, upstream: &UdpSocket, askers: &[Asker]) {
    let mut flight: Flight<(Arrival, [u8; 2])> = Flight::new(askers.len());
    let mut next: u16 = 0;
    let mut buf = [0u8; 4096];
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
        ];
        // SAFETY: poll(2) of two pollfds of ours; a second at most, so that what waits is
        // let go in time.
        if unsafe { libc::poll(set.as_mut_ptr(), 2, 1000) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return;
        }
        let now = Instant::now();
        flight.expire(now);
        while let Ok((n, from)) = receive(listen, &mut buf) {
            let q = buf.get(..n).unwrap_or_default();
            let granted = askers
                .iter()
                .find(|a| a.link == from.link)
                .is_some_and(|a| a.any || query_name(q).is_some_and(|name| a.names.contains(&name)));
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
            while flight.pending.contains_key(&next) {
                next = next.wrapping_add(1);
            }
            let mut ours = q.to_vec();
            if let Some(id) = ours.get_mut(..2) {
                id.copy_from_slice(&next.to_be_bytes());
            }
            if upstream.send(&ours).is_ok() {
                let link = from.link;
                flight.insert(next, link, (from, [i0, i1]), now);
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

    /// One asker holding its share of the IDs is refused more, and another still asks; one
    /// answered or let go has room again.
    #[test]
    fn no_asker_takes_anothers_share_of_the_ids() {
        let now = Instant::now();
        let mut f: Flight<()> = Flight::new(2);
        assert_eq!(f.share, 32767);
        for id in 0..32767u16 {
            assert!(f.admits(7));
            f.insert(id, 7, (), now);
        }
        assert!(!f.admits(7));
        assert!(f.admits(9));
        f.insert(40000, 9, (), now);
        assert!(f.remove(0).is_some());
        assert!(f.admits(7));
        f.insert(0, 7, (), now);
        f.expire(now + WAIT);
        assert!(f.admits(7) && f.pending.is_empty());
        assert_eq!(f.held.values().sum::<usize>(), 0);
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
