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
//! the uplink's chain `chosen` (links::program_chosen), programmed again whole, once every
//! call a round of poll(2) found is answered, on a netfilter socket kept for the process's
//! life (closing one waits out an RCU grace period, PM M513): what no domain listens on
//! goes nowhere past init, as a port no domain claims.
//!
//! No grant rests on this process: every candidate is granted the port by its
//! Agentfile, its gate and the switch letting it in, and the process only chooses among
//! them. A call it cannot see (another ABI's, which its filter lets through) keeps a socket
//! from being chosen, and reaches no further.

use std::io::{self, Write as _};
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
/// own) that more than one domain is given: each run of ports the same domains are given,
/// cut where any domain's range begins or ends, so that a range two domains share only in
/// part (each clipped to its own network's grants) is contested where they share it.
pub fn contested(ingress: &[(Ipv4Addr, Vec<Ingress>)]) -> Vec<Contest> {
    // Each range's first port and the one past its last, as u32, which 65535's next fits.
    let mut cuts: Vec<(u8, u32)> = ingress
        .iter()
        .flat_map(|(_, ranges)| ranges.iter())
        .flat_map(|&((proto, lo, hi), _)| [(proto, u32::from(lo)), (proto, u32::from(hi) + 1)])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut all = Vec::new();
    for pair in cuts.windows(2) {
        let [(proto, lo), (next, end)] = *pair else {
            continue;
        };
        let (Ok(lo), Ok(hi)) = (u16::try_from(lo), u16::try_from(end.saturating_sub(1))) else {
            continue;
        };
        if next != proto {
            continue;
        }
        let mut candidates: Vec<(Ipv4Addr, Egress)> = Vec::new();
        for (to, ranges) in ingress {
            let at = ranges.iter().find_map(|&((p, from_lo, from_hi), (q, at_lo, _))| {
                // The domain's port for `lo`: its own range's, at `lo`'s offset in the
                // microVM's.
                let shift = |port: u16| port.checked_sub(from_lo).and_then(|d| at_lo.checked_add(d));
                (p == proto && from_lo <= lo && hi <= from_hi)
                    .then(|| Some((q, shift(lo)?, shift(hi)?)))
                    .flatten()
            });
            if let Some(at) = at
                && !candidates.iter().any(|(a, _)| a == to)
            {
                candidates.push((*to, at));
            }
        }
        if candidates.len() > 1 {
            all.push(Contest {
                from: (proto, lo, hi),
                candidates,
            });
        }
    }
    all
}

/// The ports of `from` no contest of `contests` is for, as runs: what goes to its one
/// domain alone ([`crate::links`]' `pre`).
pub fn uncontested((proto, lo, hi): Egress, contests: &[Contest]) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let mut at = u32::from(lo);
    let mut taken: Vec<(u16, u16)> = contests
        .iter()
        .filter(|c| c.from.0 == proto && c.from.1 <= hi && lo <= c.from.2)
        .map(|c| (c.from.1, c.from.2))
        .collect();
    taken.sort_unstable();
    for (a, b) in taken {
        if u32::from(a) > at
            && let (Ok(first), Ok(last)) = (u16::try_from(at), u16::try_from(u32::from(a).saturating_sub(1)))
        {
            out.push((first, last));
        }
        at = at.max(u32::from(b) + 1);
    }
    if at <= u32::from(hi)
        && let Ok(first) = u16::try_from(at)
    {
        out.push((first, hi));
    }
    out
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

/// Why the chooser did not start.
#[derive(Debug)]
pub enum Refused {
    Zones(ZonesExhausted),
    Io(io::Error),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zones(e) => e.fmt(f),
            Self::Io(e) => e.fmt(f),
        }
    }
}

/// Starts the chooser for `contests`, before any domain does.
pub fn start(contests: Vec<Contest>) -> Result<Chooser, Refused> {
    let choices = Choices::new(contests).map_err(Refused::Zones)?;
    let (ours, theirs) = hostbind::seqpacket().map_err(Refused::Io)?;
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(Refused::Io(io::Error::last_os_error())),
        0 => {
            crate::dnsrelay::keep_only(&[theirs.as_raw_fd()]);
            serve(theirs, choices);
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
    choices: Choices,
    members: Vec<Member>,
    /// What was last programmed, so that a change alone is.
    programmed: Option<Vec<crate::links::Chosen>>,
    /// The netfilter socket choices are programmed on, kept: closing one that has
    /// committed waits out an RCU grace period (links::program_chosen).
    nft: Option<OwnedFd>,
    /// Whether a holder is to be chosen again: once every call this round of poll(2)
    /// found is answered, so that none waits behind programming another's choice.
    stale: bool,
}

/// What the chooser holds of each contest: who holds each of its ports, and the
/// conntrack zones its UDP flows are tracked in (links::program_chosen).
struct Choices {
    contests: Vec<Contest>,
    /// Each contest's ports as they are held, the microVM's: runs of them, each its first
    /// and last port and its holder's index, sorted and apart. A port goes to the member
    /// listening on it (D122), so a range's ports may be several members'.
    held: Vec<Vec<(u16, u16, usize)>>,
    /// Each UDP contest's conntrack zone for each of its candidates, for the run (none for
    /// TCP's). A zone is one holder's alone, so that every flow conntrack keeps in it is
    /// translated to that holder: a flow admitted before a change is new in the next
    /// holder's zone, and one kept from an earlier turn of the same holder goes to it
    /// again (network-04).
    zones: Vec<Vec<u16>>,
}

/// More UDP candidates than conntrack has zones for (a zone is a u16, and 0 every other
/// flow's: nf_conntrack_zones.h, NF_CT_DEFAULT_ZONE_ID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZonesExhausted {
    pub needed: usize,
}

impl std::fmt::Display for ZonesExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} UDP candidates need a conntrack zone each, and conntrack has {}",
            self.needed,
            u16::MAX
        )
    }
}

impl std::error::Error for ZonesExhausted {}

impl Choices {
    /// Each UDP candidate's zone, 1 up: zone 0 is every other flow's.
    fn new(contests: Vec<Contest>) -> Result<Self, ZonesExhausted> {
        let needed: usize = contests
            .iter()
            .filter(|c| c.from.0 == 17)
            .map(|c| c.candidates.len())
            .sum();
        let mut next = 0u16;
        let zones = contests
            .iter()
            .map(|c| {
                if c.from.0 != 17 {
                    return Ok(Vec::new());
                }
                c.candidates
                    .iter()
                    .map(|_| {
                        next = next.checked_add(1).ok_or(ZonesExhausted { needed })?;
                        Ok(next)
                    })
                    .collect()
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            held: vec![Vec::new(); contests.len()],
            zones,
            contests,
        })
    }

    /// Contest `c`'s ports, of what each candidate listens on (`listens`, by candidate,
    /// the microVM's ports, sorted): each port its holder's still, where it still listens
    /// there; else the first candidate's that does; else no one's.
    fn choose(&mut self, c: usize, listens: &[Vec<u16>]) {
        let (Some(contest), Some(held)) = (self.contests.get(c), self.held.get_mut(c)) else {
            return;
        };
        let on = |k: usize, port: u16| listens.get(k).is_some_and(|l| l.binary_search(&port).is_ok());
        let mut ports: Vec<u16> = listens.iter().flatten().copied().collect();
        ports.sort_unstable();
        ports.dedup();
        let mut next: Vec<(u16, u16, usize)> = Vec::new();
        for port in ports {
            let was = held
                .iter()
                .find(|&&(lo, hi, _)| lo <= port && port <= hi)
                .map(|&(_, _, k)| k);
            let Some(k) = was
                .filter(|&k| on(k, port))
                .or_else(|| (0..contest.candidates.len()).find(|&k| on(k, port)))
            else {
                continue;
            };
            match next.last_mut() {
                Some((_, hi, by)) if *by == k && hi.checked_add(1) == Some(port) => *hi = port,
                _ => next.push((port, port, k)),
            }
        }
        *held = next;
    }

    /// The choices, as the chain `chosen` programs them: each run of a contest's ports to
    /// its holder, at the holder's own ports, in the holder's zone where they are UDP's.
    fn chosen(&self) -> Vec<crate::links::Chosen> {
        let mut out = Vec::new();
        for ((contest, held), zones) in self.contests.iter().zip(&self.held).zip(&self.zones) {
            let (proto, first, _) = contest.from;
            for &(lo, hi, k) in held {
                let Some(&(addr, at)) = contest.candidates.get(k) else {
                    continue;
                };
                // The holder's port: its range's, at the port's offset in the microVM's.
                let theirs = |port: u16| port.checked_sub(first).and_then(|d| at.1.checked_add(d));
                let (Some(alo), Some(ahi)) = (theirs(lo), theirs(hi)) else {
                    continue;
                };
                out.push(((proto, lo, hi), addr, (at.0, alo, ahi), zones.get(k).copied()));
            }
        }
        out
    }
}

#[derive(Clone, Copy)]
enum Owner {
    Ctl,
    Listener(usize),
    Told(usize),
}

fn serve(ctl: OwnedFd, choices: Choices) {
    let mut s = Serve {
        ctl,
        choices,
        members: Vec::new(),
        programmed: None,
        nft: None,
        stale: false,
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
                        s.stale = true;
                    }
                }
                Owner::Told(i) => s.told(i),
            }
        }
        if std::mem::take(&mut s.stale) {
            s.choose_all();
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
                match in_namespace(&fd) {
                    Ok((ask, told, unprivileged)) => {
                        m.ask = Some(ask);
                        m.told = Some(told);
                        m.unprivileged = unprivileged;
                    }
                    // Its sockets cannot be asked of: it is chosen for nothing, and said so.
                    Err(e) => {
                        let _ = writeln!(
                            io::stderr(),
                            "shards-init: the ports' chooser: {addr}'s namespace: {e}"
                        );
                    }
                }
                self.stale = true;
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
        self.stale |= changed;
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
        self.choices.contests.iter().any(|c| {
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
        self.stale |= any;
    }

    /// Each contest's ports, as its candidates listen on them ([`Choices::choose`]); then
    /// the chain programmed, where any changed.
    fn choose_all(&mut self) {
        for c in 0..self.choices.contests.len() {
            let Some(contest) = self.choices.contests.get(c) else {
                continue;
            };
            let listens: Vec<Vec<u16>> = contest
                .candidates
                .iter()
                .map(|&(addr, at)| {
                    self.members
                        .iter()
                        .find(|m| m.addr == addr)
                        .and_then(|m| m.ask.as_ref())
                        .map(|ask| listened(ask, addr, at))
                        .unwrap_or_default()
                        .into_iter()
                        // The member's port, as the microVM's: its offset in the range.
                        .filter_map(|q| q.checked_sub(at.1).and_then(|d| contest.from.1.checked_add(d)))
                        .collect()
                })
                .collect();
            self.choices.choose(c, &listens);
        }
        self.program();
    }

    fn program(&mut self) {
        let chosen = self.choices.chosen();
        if self.programmed.as_ref() == Some(&chosen) {
            return;
        }
        let nft = match self.nft.take() {
            Some(nft) => Ok(nft),
            None => crate::links::netlink_socket(libc::NETLINK_NETFILTER),
        };
        // Kept while it programs; one that failed, opened again for the next choice.
        let done = nft.and_then(|nft| {
            crate::links::program_chosen(&nft, &chosen)?;
            self.nft = Some(nft);
            Ok(())
        });
        match done {
            Ok(()) => self.programmed = Some(chosen),
            Err(e) => {
                let _ = writeln!(
                    io::stderr(),
                    "shards-init: the ports' chooser: programming its choices: {e}"
                );
            }
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

/// The ports of `at` at which a socket of `ask`'s namespace takes what comes to `addr`: a
/// TCP one listening, a UDP one bound and connected to no peer; at that address, or every
/// one (IPv4's, or IPv6's, which takes IPv4's unless it is IPV6_V6ONLY). Sorted, each once.
fn listened(ask: &OwnedFd, addr: Ipv4Addr, (proto, lo, hi): Egress) -> Vec<u16> {
    // TCP_LISTEN's state bit; every state of UDP's.
    let states: u32 = if proto == 6 { 1 << 10 } else { u32::MAX };
    let mut buf = vec![0u8; 16 * 1024];
    let mut found = Vec::new();
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
            break;
        }
        'dump: loop {
            // SAFETY: recv(2) into a buffer of ours, of its length.
            let n = unsafe { libc::recv(ask.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            let Some(n) = usize::try_from(n).ok().filter(|&n| n > 0) else {
                break 'dump;
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
                    Some(hostbind::NLMSG_ERROR) => break 'dump,
                    _ => found.extend(msgs.get(..len).and_then(|m| taken(m, family, addr, (lo, hi)))),
                }
                msgs = msgs.get(len.next_multiple_of(4)..).unwrap_or_default();
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The port of `lo..=hi` at which one sock_diag message (an nlmsghdr, then an
/// inet_diag_msg: family, state, timer, retransmits; its sport and dport, big-endian; its
/// source and destination, 16 bytes each) names a socket of `family`, at `addr` or every
/// address, connected to no peer.
fn taken(m: &[u8], family: libc::c_int, addr: Ipv4Addr, (lo, hi): (u16, u16)) -> Option<u16> {
    let be16 = |at: usize| {
        m.get(at..at + 2)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(u16::from_be_bytes)
    };
    let (sport, dport) = (be16(20)?, be16(22)?);
    let src = m.get(24..40)?;
    if sport < lo || hi < sport || dport != 0 {
        return None;
    }
    let src = if family == libc::AF_INET {
        let &[a, b, c, d] = src.first_chunk::<4>()?;
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    } else {
        IpAddr::V6(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(src).ok()?))
    };
    let at = match src {
        IpAddr::V4(a) => a.is_unspecified() || a == addr,
        IpAddr::V6(a) => a.is_unspecified() || a.to_ipv4_mapped() == Some(addr),
    };
    at.then_some(sport)
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

    /// Two domains' ranges that meet in part, each clipped to its own network's grants:
    /// contested where they meet, each the rest's alone.
    #[test]
    fn ranges_that_meet_in_part_are_contested_where_they_meet() {
        let (a, b) = (Ipv4Addr::new(10, 0, 0, 2), Ipv4Addr::new(10, 0, 1, 2));
        let ingress = vec![
            (a, vec![((6, 7000, 7002), (6, 7000, 7002))]),
            (b, vec![((6, 7001, 7001), (6, 7001, 7001)), ((17, 7000, 7002), (17, 7000, 7002))]),
        ];
        let contests = contested(&ingress);
        assert_eq!(
            contests,
            [Contest {
                from: (6, 7001, 7001),
                candidates: vec![(a, (6, 7001, 7001)), (b, (6, 7001, 7001))],
            }]
        );
        assert_eq!(uncontested((6, 7000, 7002), &contests), [(7000, 7000), (7002, 7002)]);
        assert_eq!(uncontested((6, 7001, 7001), &contests), []);
        assert_eq!(uncontested((17, 7000, 7002), &contests), [(7000, 7002)]);
        // At the port space's end, and a mapped port.
        let ingress = vec![
            (a, vec![((17, 65534, 65535), (17, 65534, 65535))]),
            (b, vec![((17, 65535, 65535), (17, 53, 53))]),
        ];
        let contests = contested(&ingress);
        assert_eq!(
            contests,
            [Contest {
                from: (17, 65535, 65535),
                candidates: vec![(a, (17, 65535, 65535)), (b, (17, 53, 53))],
            }]
        );
        assert_eq!(uncontested((17, 65534, 65535), &contests), [(65534, 65534)]);
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
        assert_eq!(taken(&msg(2, 8080, 0, &[0, 0, 0, 0]), v4, addr, (8080, 8081)), Some(8080));
        assert_eq!(taken(&msg(2, 8081, 0, &[10, 0, 0, 2]), v4, addr, (8080, 8081)), Some(8081));
        assert_eq!(taken(&msg(2, 8080, 0, &[127, 0, 0, 1]), v4, addr, (8080, 8080)), None);
        assert_eq!(taken(&msg(2, 8081, 0, &[0, 0, 0, 0]), v4, addr, (8080, 8080)), None);
        assert_eq!(taken(&msg(2, 8080, 443, &[0, 0, 0, 0]), v4, addr, (8080, 8080)), None);
        let mapped = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 10, 0, 0, 2];
        let v6 = libc::AF_INET6;
        assert_eq!(taken(&msg(10, 8080, 0, &[0; 16]), v6, addr, (8080, 8080)), Some(8080));
        assert_eq!(taken(&msg(10, 8080, 0, &mapped), v6, addr, (8080, 8080)), Some(8080));
    }

    /// network-04: a UDP port's holders handed it back and forth past the zones a u16
    /// counts. Expect: each zone names one holder for the run, so that a flow conntrack
    /// still keeps in a zone is translated to the holder that zone is for.
    #[test]
    fn a_udp_zone_names_one_holder_for_the_run() {
        let (a, b) = (Ipv4Addr::new(10, 0, 0, 2), Ipv4Addr::new(10, 0, 1, 2));
        let dns = (17, 5353, 5353);
        let mut choices = Choices::new(vec![Contest {
            from: dns,
            candidates: vec![(a, dns), (b, dns)],
        }])
        .unwrap();
        let mut held_by = std::collections::HashMap::new();
        for round in 0..=usize::from(u16::MAX) + 1 {
            let listens = if round % 2 == 0 {
                [vec![5353], vec![]]
            } else {
                [vec![], vec![5353]]
            };
            choices.choose(0, &listens);
            let chosen = choices.chosen();
            let [(_, to, _, Some(zone))] = chosen[..] else {
                panic!("round {round}: one zoned choice, got {chosen:?}");
            };
            assert_ne!(zone, 0, "the default zone is every other flow's");
            let first = *held_by.entry(zone).or_insert(to);
            assert_eq!(first, to, "round {round}: zone {zone} was {first}'s, given {to}");
        }
    }

    /// More UDP candidates than zones: refused, never a zone shared or zone 0.
    #[test]
    fn more_udp_candidates_than_zones_are_refused() {
        let at = |i: u32| Ipv4Addr::from(0x0a00_0000 + i);
        let contest = |port: u16, n: u32| Contest {
            from: (17, port, port),
            candidates: (0..n).map(|i| (at(i), (17, port, port))).collect(),
        };
        let max = u32::from(u16::MAX);
        assert!(Choices::new(vec![contest(53, max - 1), contest(54, 1)]).is_ok());
        assert_eq!(
            Choices::new(vec![contest(53, max), contest(54, 1)]).err(),
            Some(ZonesExhausted {
                needed: usize::from(u16::MAX) + 1
            })
        );
        // TCP's contests take none.
        let tcp = Contest {
            from: (6, 80, 80),
            candidates: vec![(at(1), (6, 80, 80)), (at(2), (6, 80, 80))],
        };
        assert!(Choices::new(vec![contest(53, max), tcp]).is_ok());
    }

    /// network-06: web listens on 7000 alone, api on 7001 alone, both candidates for
    /// 7000-7001; then api's socket goes. Expect: each port to the member listening on it,
    /// then 7001 to no one.
    #[test]
    fn each_port_of_a_range_goes_to_the_member_listening_on_it() {
        let (web, api) = (Ipv4Addr::new(10, 0, 0, 2), Ipv4Addr::new(10, 0, 1, 2));
        for proto in [6u8, 17] {
            let range = (proto, 7000, 7001);
            let mut choices = Choices::new(vec![Contest {
                from: range,
                candidates: vec![(web, range), (api, range)],
            }])
            .unwrap();
            choices.choose(0, &[vec![7000], vec![7001]]);
            let chosen = choices.chosen();
            let to: Vec<_> = chosen.iter().map(|&(from, to, at, _)| (from, to, at)).collect();
            assert_eq!(
                to,
                [
                    ((proto, 7000, 7000), web, (proto, 7000, 7000)),
                    ((proto, 7001, 7001), api, (proto, 7001, 7001)),
                ],
                "{proto}"
            );
            if proto == 17 {
                assert_ne!(chosen[0].3, chosen[1].3, "two holders share a zone");
            }
            choices.choose(0, &[vec![7000], vec![]]);
            let to: Vec<_> = choices.chosen().iter().map(|&(from, to, _, _)| (from, to)).collect();
            assert_eq!(to, [((proto, 7000, 7000), web)], "{proto}");
        }
    }

    /// A mapped port's holder is reached at its own port; adjacent ports one member listens
    /// on are one range.
    #[test]
    fn a_choice_keeps_its_members_port_and_joins_adjacent_ports() {
        let (web, api) = (Ipv4Addr::new(10, 0, 0, 2), Ipv4Addr::new(10, 0, 1, 2));
        let mut choices = Choices::new(vec![
            Contest {
                from: (6, 8080, 8080),
                candidates: vec![(web, (6, 80, 80)), (api, (6, 8080, 8080))],
            },
            Contest {
                from: (6, 9000, 9003),
                candidates: vec![(web, (6, 9000, 9003)), (api, (6, 9000, 9003))],
            },
        ])
        .unwrap();
        choices.choose(0, &[vec![8080], vec![8080]]);
        choices.choose(1, &[vec![9000, 9001, 9003], vec![9001, 9002]]);
        assert_eq!(
            choices.chosen(),
            [
                ((6, 8080, 8080), web, (6, 80, 80), None),
                ((6, 9000, 9001), web, (6, 9000, 9001), None),
                ((6, 9002, 9002), api, (6, 9002, 9002), None),
                ((6, 9003, 9003), web, (6, 9003, 9003), None),
            ]
        );
        // 9001's holder keeps it while it listens, though web, declared first, listens too.
        choices.choose(1, &[vec![9000], vec![9001]]);
        choices.choose(1, &[vec![9000, 9001], vec![9001]]);
        let to: Vec<_> = choices.chosen().iter().map(|&(from, to, _, _)| (from, to)).collect();
        assert_eq!(to[1..], [((6, 9000, 9000), web), ((6, 9001, 9001), api)]);
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
