//! The member listening on a port coming in (D122, the user's decision of 2026-11-08):
//! where an Agentfile gives one port of its microVM's to several of its domains (a
//! network's every member where its `EXPOSE` names none, or several named), each
//! connection, or datagram, goes to the domain listening on it, as a host's port goes to
//! the one socket bound there (RFC 9293 §3.10.7.2: a SYN is for the socket that is in
//! LISTEN on its port; bind(2)'s EADDRINUSE keeps a second): the first of them to listen
//! holds it while it listens, then the next that does, in the order they are declared.
//!
//! A process of init's own, in its network namespace, as the host network's binds are
//! (hostbind.rs; init forks it single-threaded, before the domains start): each candidate
//! domain runs under a filter that notifies of each listen(2) and bind(2) ([`filter`]),
//! whose listener the domain sends init as it starts and init this process, with the
//! domain's network namespace. A TCP socket's listen this process makes itself, so that it
//! knows the socket listens as it answers; a UDP socket's bind to a port a contest is
//! for, too, refused as the domain's own kernel would refuse it (a port below the
//! namespace's `ip_unprivileged_port_start` without CAP_NET_BIND_SERVICE, EACCES). As a
//! domain's socket goes, sock_diag tells this process (SKNLGRP_*_DESTROY, a socket in each
//! domain's namespace), and the port goes to the next that listens, if any. Each choice is
//! the uplink's chain `chosen` (links::program_chosen), programmed again whole: what no
//! domain listens on goes nowhere past init, as a port no domain claims.
//!
//! No grant rests on this process: every candidate is granted the port by its
//! Agentfile, its gate and the switch letting it in, and the process only chooses among
//! them. A call it cannot see (another ABI's, which its filter lets through) keeps a socket
//! from being chosen, and reaches no further.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::hostbind::{self, Call};
use crate::netplan::{Egress, Ingress};

/// A port several domains are given: the microVM's range, and each domain's address and
/// its range there, in the order the domains are declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contest {
    pub from: Egress,
    pub candidates: Vec<(Ipv4Addr, Egress)>,
}

/// The ports of `ingress` (each domain's address and its ports, the microVM's then its
/// own) that more than one domain is given.
pub fn contested(ingress: &[(Ipv4Addr, Vec<Ingress>)]) -> Vec<Contest> {
    let mut all: Vec<Contest> = Vec::new();
    for (to, ranges) in ingress {
        for &(from, at) in ranges {
            match all.iter_mut().find(|c| c.from == from) {
                Some(c) => {
                    if !c.candidates.iter().any(|(a, _)| a == to) {
                        c.candidates.push((*to, at));
                    }
                }
                None => all.push(Contest {
                    from,
                    candidates: vec![(*to, at)],
                }),
            }
        }
    }
    all.retain(|c| c.candidates.len() > 1);
    all
}

/// seccomp_data's offsets (linux/seccomp.h): the call's number, then its arch.
const NR_AT: u32 = 0;
const ARCH_AT: u32 = 4;
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
/// SECCOMP_FILTER_FLAG_NEW_LISTENER: the load returns the filter's listener.
pub const NEW_LISTENER: libc::c_long = 1 << 3;

/// This architecture's arch and its numbers for bind(2) and listen(2)
/// (arch/x86/entry/syscalls/syscall_64.tbl; include/uapi/asm-generic/unistd.h).
#[cfg(target_arch = "x86_64")]
const NATIVE: (u32, u32, u32) = (0xc000_003e, 49, 50);
#[cfg(target_arch = "aarch64")]
const NATIVE: (u32, u32, u32) = (0xc000_00b7, 200, 201);

/// The candidates' filter: bind(2) and listen(2) of the native ABI notified, all else
/// let through.
pub fn filter() -> [libc::sock_filter; 8] {
    let (arch, bind, listen) = NATIVE;
    let op = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    [
        op(BPF_LD_W_ABS, 0, 0, ARCH_AT),
        op(BPF_JEQ_K, 0, 4, arch),
        op(BPF_LD_W_ABS, 0, 0, NR_AT),
        op(BPF_JEQ_K, 3, 0, listen),
        op(BPF_JEQ_K, 2, 0, bind),
        op(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
        op(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
        op(BPF_RET_K, 0, 0, SECCOMP_RET_USER_NOTIF),
    ]
}

/// What init sends: a domain's listener, then its network namespace, each with its
/// address.
const LISTENER: u8 = b'L';
const NETNS: u8 = b'N';

/// Init's end of the chooser. Dropped, the chooser's ends: its candidates' calls are
/// refused as a listener no one holds refuses them.
#[derive(Debug)]
pub struct Chooser {
    sock: OwnedFd,
}

/// Starts the chooser for `contests`, before any domain does.
pub fn start(contests: Vec<Contest>) -> io::Result<Chooser> {
    let (ours, theirs) = hostbind::seqpacket()?;
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            crate::dnsrelay::keep_only(&[theirs.as_raw_fd()]);
            serve(theirs, contests);
            // SAFETY: _exit(2) of the child, which owns nothing to flush.
            unsafe { libc::_exit(0) }
        }
        _ => Ok(Chooser { sock: ours }),
    }
}

impl Chooser {
    /// Gives the chooser candidate `addr`'s listener and network namespace: false where
    /// it has gone.
    pub fn give(&self, addr: Ipv4Addr, listener: &OwnedFd, netns: &OwnedFd) -> bool {
        let [a, b, c, d] = addr.octets();
        hostbind::send_fd(
            self.sock.as_raw_fd(),
            &[LISTENER, a, b, c, d],
            listener.as_raw_fd(),
            0,
        ) && hostbind::send_fd(self.sock.as_raw_fd(), &[NETNS, a, b, c, d], netns.as_raw_fd(), 0)
    }
}

/// A candidate domain: its listener, and sock_diag sockets in its network namespace, one
/// to ask, one told of each socket's end; the lowest port its namespace lets any process
/// bind; and whether it has both yet.
struct Member {
    addr: Ipv4Addr,
    listener: Option<OwnedFd>,
    ask: Option<OwnedFd>,
    told: Option<OwnedFd>,
    unprivileged: u16,
}

struct Serve {
    ctl: OwnedFd,
    contests: Vec<Contest>,
    /// Each contest's holder, by its candidate's index.
    holders: Vec<Option<usize>>,
    members: Vec<Member>,
    /// What was last programmed, so that a change alone is.
    programmed: Option<Vec<(Egress, Ipv4Addr, Egress)>>,
}

#[derive(Clone, Copy)]
enum Owner {
    Ctl,
    Listener(usize),
    Told(usize),
}

fn serve(ctl: OwnedFd, contests: Vec<Contest>) {
    let holders = vec![None; contests.len()];
    let mut s = Serve {
        ctl,
        contests,
        holders,
        members: Vec::new(),
        programmed: None,
    };
    s.program();
    let mut watched: Vec<(RawFd, Owner)> = Vec::new();
    let mut set: Vec<libc::pollfd> = Vec::new();
    loop {
        watched.clear();
        watched.push((s.ctl.as_raw_fd(), Owner::Ctl));
        for (i, m) in s.members.iter().enumerate() {
            if let Some(l) = &m.listener {
                watched.push((l.as_raw_fd(), Owner::Listener(i)));
            }
            if let Some(t) = &m.told {
                watched.push((t.as_raw_fd(), Owner::Told(i)));
            }
        }
        set.clear();
        set.extend(watched.iter().map(|&(fd, _)| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }));
        // SAFETY: poll(2) on pollfds of descriptors this process holds, as many as given.
        if unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        for (p, &(_, owner)) in set.iter().zip(&watched).filter(|(p, _)| p.revents != 0) {
            match owner {
                Owner::Ctl => {
                    if !s.heard() {
                        // Init gone: the run has ended.
                        return;
                    }
                }
                Owner::Listener(i) => {
                    if p.revents & libc::POLLIN != 0 {
                        s.notified(i);
                    } else {
                        // Its processes are gone: it listens on nothing.
                        if let Some(m) = s.members.get_mut(i) {
                            (m.listener, m.ask, m.told) = (None, None, None);
                        }
                        s.choose_all();
                    }
                }
                Owner::Told(i) => s.told(i),
            }
        }
    }
}

impl Serve {
    /// What init has sent; false once it has gone.
    fn heard(&mut self) -> bool {
        let mut buf = [0u8; 16];
        loop {
            match hostbind::recv_fd(self.ctl.as_raw_fd(), &mut buf, libc::MSG_DONTWAIT) {
                Ok((0, _)) => return false,
                Ok((n, Some(fd))) => {
                    if let Some(&[which, a, b, c, d]) = buf.get(..n) {
                        self.member(which, Ipv4Addr::new(a, b, c, d), fd);
                    }
                }
                Ok(_) => {}
                Err(e) => return e.kind() == io::ErrorKind::WouldBlock,
            }
        }
    }

    /// A candidate's listener, or its network namespace, in which its sock_diag sockets
    /// are made and its lowest unprivileged port read.
    fn member(&mut self, which: u8, addr: Ipv4Addr, fd: OwnedFd) {
        let at = match self.members.iter().position(|m| m.addr == addr) {
            Some(at) => at,
            None => {
                self.members.push(Member {
                    addr,
                    listener: None,
                    ask: None,
                    told: None,
                    unprivileged: 1024,
                });
                self.members.len() - 1
            }
        };
        let Some(m) = self.members.get_mut(at) else { return };
        match which {
            LISTENER => m.listener = Some(fd),
            NETNS => {
                if let Ok((ask, told, unprivileged)) = in_namespace(&fd) {
                    m.ask = Some(ask);
                    m.told = Some(told);
                    m.unprivileged = unprivileged;
                }
                self.choose_all();
            }
            _ => {}
        }
    }

    /// One notification of member `i`'s listener, answered.
    fn notified(&mut self, i: usize) {
        let Some(listener) = self
            .members
            .get(i)
            .and_then(|m| m.listener.as_ref())
            .map(AsRawFd::as_raw_fd)
        else {
            return;
        };
        // SAFETY: zeroed is a valid seccomp_notif; the kernel fills it.
        let mut n: libc::seccomp_notif = unsafe { std::mem::zeroed() };
        // SAFETY: ioctl(2) receiving into a struct of the size its number says.
        if unsafe { libc::ioctl(listener, libc::SECCOMP_IOCTL_NOTIF_RECV, &raw mut n) } != 0 {
            return;
        }
        let (answer, changed) = self.take(i, listener, &n);
        let mut resp = libc::seccomp_notif_resp {
            id: n.id,
            val: 0,
            error: 0,
            flags: 0,
        };
        match answer {
            Some(Ok(())) => {}
            Some(Err(e)) => resp.error = -e,
            None => resp.flags = libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
        }
        // SAFETY: ioctl(2) sending a struct of the size its number says.
        unsafe { libc::ioctl(listener, libc::SECCOMP_IOCTL_NOTIF_SEND, &raw const resp) };
        if changed {
            self.choose_all();
        }
    }

    /// The call of notification `n` of member `i`'s: made here, its result (none to let
    /// the kernel make it), and whether a contest's port is now listened on.
    fn take(&self, i: usize, listener: RawFd, n: &libc::seccomp_notif) -> (Option<Result<(), i32>>, bool) {
        let Some(m) = self.members.get(i) else {
            return (None, false);
        };
        let (Some(call), Ok(pid)) = (hostbind::call(n), libc::pid_t::try_from(n.pid)) else {
            return (None, false);
        };
        let (Call::Bind { fd, .. } | Call::Listen { fd, .. }) = call;
        let Some(sock) = hostbind::socket_of(pid, fd) else {
            return (None, false);
        };
        let Some((domain, proto)) = hostbind::inet(&sock) else {
            return (None, false);
        };
        match call {
            Call::Listen { backlog, .. } if proto == libc::IPPROTO_TCP as u8 => {
                let Some(port) = hostbind::local_port(&sock).filter(|&p| p != 0) else {
                    return (None, false);
                };
                if !self.contests_of(m.addr, 6, port) || !hostbind::valid(listener, n.id) {
                    return (None, false);
                }
                // SAFETY: listen(2) of a socket of ours, the domain's.
                if unsafe { libc::listen(sock.as_raw_fd(), backlog as libc::c_int) } != 0 {
                    return (Some(Err(hostbind::errno())), false);
                }
                (Some(Ok(())), true)
            }
            Call::Bind { addr, len, .. } if proto == libc::IPPROTO_UDP as u8 => {
                let Some((ip, port)) = hostbind::read_sockaddr(pid, addr, len, domain) else {
                    return (None, false);
                };
                if port == 0 || !self.contests_of(m.addr, 17, port) || !hostbind::valid(listener, n.id) {
                    return (None, false);
                }
                // The domain's own kernel's refusal of a privileged port (inet_bind_sk,
                // inet_port_requires_bind_service), which this process's capabilities
                // would not make.
                if port < m.unprivileged && !binds_service(pid) {
                    return (Some(Err(libc::EACCES)), false);
                }
                match hostbind::bind_socket(&sock, ip, port) {
                    Ok(()) => (Some(Ok(())), true),
                    Err(e) => (Some(Err(e)), false),
                }
            }
            _ => (None, false),
        }
    }

    /// Whether `port` of `proto` is one member `addr` is a candidate for, at its own port.
    fn contests_of(&self, addr: Ipv4Addr, proto: u8, port: u16) -> bool {
        self.contests.iter().any(|c| {
            c.candidates
                .iter()
                .any(|&(a, (p, lo, hi))| a == addr && p == proto && lo <= port && port <= hi)
        })
    }

    /// What member `i`'s namespace says of its sockets' ends: each contest it holds asked
    /// again.
    fn told(&mut self, i: usize) {
        let Some(fd) = self
            .members
            .get(i)
            .and_then(|m| m.told.as_ref())
            .map(AsRawFd::as_raw_fd)
        else {
            return;
        };
        let mut buf = [0u8; 8192];
        let mut any = false;
        loop {
            // SAFETY: recv(2) into a buffer of ours, of its length.
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
            if n <= 0 {
                break;
            }
            any = true;
        }
        if any {
            self.choose_all();
        }
    }

    /// Each contest's holder: its holder still, where it still listens; else the first of
    /// its candidates that does; then the chain programmed, where any changed.
    fn choose_all(&mut self) {
        for c in 0..self.contests.len() {
            let Some(contest) = self.contests.get(c) else {
                continue;
            };
            let listens = |k: usize| {
                contest.candidates.get(k).is_some_and(|&(addr, at)| {
                    self.members
                        .iter()
                        .find(|m| m.addr == addr)
                        .and_then(|m| m.ask.as_ref())
                        .is_some_and(|ask| listening(ask, addr, at))
                })
            };
            let held = self.holders.get(c).copied().flatten().filter(|&k| listens(k));
            let holder = held.or_else(|| (0..contest.candidates.len()).find(|&k| listens(k)));
            if let Some(h) = self.holders.get_mut(c) {
                *h = holder;
            }
        }
        self.program();
    }

    fn program(&mut self) {
        let chosen: Vec<(Egress, Ipv4Addr, Egress)> = self
            .contests
            .iter()
            .zip(&self.holders)
            .filter_map(|(c, h)| {
                let (addr, at) = c.candidates.get((*h)?)?;
                Some((c.from, *addr, *at))
            })
            .collect();
        if self.programmed.as_ref() == Some(&chosen) {
            return;
        }
        if crate::links::program_chosen(&chosen).is_ok() {
            self.programmed = Some(chosen);
        }
    }
}

/// In the namespace `netns`, on a thread of its own: a sock_diag socket to ask, one told
/// of each socket's end, and `ip_unprivileged_port_start`.
fn in_namespace(netns: &OwnedFd) -> io::Result<(OwnedFd, OwnedFd, u16)> {
    crate::links::in_netns(Some(netns.as_raw_fd()), || {
        let ask = hostbind::diag_socket()?;
        let told = hostbind::destroy_listener()?;
        let unprivileged = std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .unwrap_or(1024);
        Ok((ask, told, unprivileged))
    })
}

/// Whether process `pid` holds CAP_NET_BIND_SERVICE (10), as its status says.
fn binds_service(pid: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("CapEff:"))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        })
        .is_some_and(|caps| caps & (1 << 10) != 0)
}

/// Whether a socket of `ask`'s namespace takes what comes to `addr` at a port of `at`: a
/// TCP one listening, a UDP one bound and connected to no peer; at that address, or every
/// one (IPv4's, or IPv6's, which takes IPv4's unless it is IPV6_V6ONLY).
fn listening(ask: &OwnedFd, addr: Ipv4Addr, (proto, lo, hi): Egress) -> bool {
    // TCP_LISTEN's state bit; every state of UDP's.
    let states: u32 = if proto == 6 { 1 << 10 } else { u32::MAX };
    let mut buf = vec![0u8; 16 * 1024];
    for family in [libc::AF_INET, libc::AF_INET6] {
        // nlmsghdr (16), then inet_diag_req_v2: family, protocol, extensions, pad, states,
        // and its socket ID, zeroed (48).
        let mut req = Vec::with_capacity(72);
        req.extend_from_slice(&72u32.to_ne_bytes());
        req.extend_from_slice(&hostbind::SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        req.extend_from_slice(&(hostbind::NLM_F_REQUEST | hostbind::NLM_F_DUMP).to_ne_bytes());
        req.extend_from_slice(&[0; 8]);
        req.extend_from_slice(&[family as u8, proto, 0, 0]);
        req.extend_from_slice(&states.to_ne_bytes());
        req.extend_from_slice(&[0; 48]);
        // SAFETY: send(2) of a buffer of ours, to the kernel.
        if unsafe { libc::send(ask.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) } < 0 {
            return false;
        }
        let mut found = false;
        'dump: loop {
            // SAFETY: recv(2) into a buffer of ours, of its length.
            let n = unsafe { libc::recv(ask.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            let Some(n) = usize::try_from(n).ok().filter(|&n| n > 0) else {
                return found;
            };
            let mut msgs = buf.get(..n).unwrap_or_default();
            while let Some(len) = msgs
                .get(..4)
                .and_then(|l| <[u8; 4]>::try_from(l).ok())
                .map(u32::from_ne_bytes)
                .and_then(|l| usize::try_from(l).ok())
                .filter(|&l| l >= 16)
            {
                let which = msgs
                    .get(4..6)
                    .and_then(|k| <[u8; 2]>::try_from(k).ok())
                    .map(u16::from_ne_bytes);
                match which {
                    Some(hostbind::NLMSG_DONE) => break 'dump,
                    Some(hostbind::NLMSG_ERROR) => return found,
                    _ => {
                        if msgs.get(..len).is_some_and(|m| takes(m, family, addr, (lo, hi))) {
                            found = true;
                        }
                    }
                }
                msgs = msgs.get(len.next_multiple_of(4)..).unwrap_or_default();
            }
        }
        if found {
            return true;
        }
    }
    false
}

/// Whether one sock_diag message (an nlmsghdr, then an inet_diag_msg: family, state,
/// timer, retransmits; its sport and dport, big-endian; its source and destination, 16
/// bytes each) names a socket of `family` at a port of `lo..=hi`, at `addr` or every
/// address, connected to no peer.
fn takes(m: &[u8], family: libc::c_int, addr: Ipv4Addr, (lo, hi): (u16, u16)) -> bool {
    let be16 = |at: usize| {
        m.get(at..at + 2)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(u16::from_be_bytes)
    };
    let (Some(sport), Some(dport)) = (be16(20), be16(22)) else {
        return false;
    };
    let Some(src) = m.get(24..40) else {
        return false;
    };
    if sport < lo || hi < sport || dport != 0 {
        return false;
    }
    let src = if family == libc::AF_INET {
        let Some(&[a, b, c, d]) = src.first_chunk::<4>() else {
            return false;
        };
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    } else {
        let Ok(a) = <[u8; 16]>::try_from(src) else {
            return false;
        };
        IpAddr::V6(std::net::Ipv6Addr::from(a))
    };
    match src {
        IpAddr::V4(a) => a.is_unspecified() || a == addr,
        IpAddr::V6(a) => a.is_unspecified() || a.to_ipv4_mapped() == Some(addr),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// A port two domains are given is contested, each its own port there; one a domain
    /// alone has is not.
    #[test]
    fn ports_several_domains_are_given_are_contested() {
        let (a, b) = (Ipv4Addr::new(10, 0, 0, 2), Ipv4Addr::new(10, 0, 1, 2));
        let web = ((6, 8080, 8080), (6, 8080, 8080));
        let ingress = vec![
            (a, vec![web, ((6, 9000, 9000), (6, 9000, 9000))]),
            (b, vec![((6, 8080, 8080), (6, 80, 80))]),
        ];
        assert_eq!(
            contested(&ingress),
            [Contest {
                from: (6, 8080, 8080),
                candidates: vec![(a, (6, 8080, 8080)), (b, (6, 80, 80))],
            }]
        );
        assert!(contested(&ingress[..1]).is_empty());
    }

    /// A socket is taken for an address where sock_diag names it at that address or every
    /// one, at a port of the range, connected to no one.
    #[test]
    fn a_socket_takes_what_comes_to_its_address_and_port() {
        let addr = Ipv4Addr::new(10, 0, 0, 2);
        let msg = |family: u8, sport: u16, dport: u16, src: &[u8]| {
            let mut m = vec![0u8; 16];
            m.extend_from_slice(&[family, 10, 0, 0]);
            m.extend_from_slice(&sport.to_be_bytes());
            m.extend_from_slice(&dport.to_be_bytes());
            let mut s = [0u8; 16];
            s[..src.len()].copy_from_slice(src);
            m.extend_from_slice(&s);
            m.extend_from_slice(&[0u8; 16]);
            m
        };
        let v4 = libc::AF_INET;
        assert!(takes(&msg(2, 8080, 0, &[0, 0, 0, 0]), v4, addr, (8080, 8080)));
        assert!(takes(&msg(2, 8080, 0, &[10, 0, 0, 2]), v4, addr, (8080, 8080)));
        assert!(!takes(&msg(2, 8080, 0, &[127, 0, 0, 1]), v4, addr, (8080, 8080)));
        assert!(!takes(&msg(2, 8081, 0, &[0, 0, 0, 0]), v4, addr, (8080, 8080)));
        assert!(!takes(&msg(2, 8080, 443, &[0, 0, 0, 0]), v4, addr, (8080, 8080)));
        let mapped = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 10, 0, 0, 2];
        assert!(takes(
            &msg(10, 8080, 0, &[0; 16]),
            libc::AF_INET6,
            addr,
            (8080, 8080)
        ));
        assert!(takes(
            &msg(10, 8080, 0, &mapped),
            libc::AF_INET6,
            addr,
            (8080, 8080)
        ));
    }

    /// The filter notifies of the native ABI's bind and listen alone.
    #[test]
    fn the_filter_notifies_of_bind_and_listen_alone() {
        let f = filter();
        let run = |arch: u32, nr: u32| -> u32 {
            let data = |at: u32| if at == ARCH_AT { arch } else { nr };
            let mut pc = 0usize;
            let mut acc = 0u32;
            loop {
                let i = f[pc];
                match i.code {
                    BPF_LD_W_ABS => {
                        acc = data(i.k);
                        pc += 1;
                    }
                    BPF_JEQ_K => pc += 1 + usize::from(if acc == i.k { i.jt } else { i.jf }),
                    _ => return i.k,
                }
            }
        };
        let (arch, bind, listen) = NATIVE;
        assert_eq!(run(arch, bind), SECCOMP_RET_USER_NOTIF);
        assert_eq!(run(arch, listen), SECCOMP_RET_USER_NOTIF);
        assert_eq!(run(arch, 0), SECCOMP_RET_ALLOW);
        assert_eq!(run(arch ^ 1, listen), SECCOMP_RET_ALLOW);
    }
}
