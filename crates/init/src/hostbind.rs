//! A run on the host's network (`--network host`, D124): each bind(2) and listen(2) of its
//! processes made the host's, by a process of init's own, as its DNS relays are (init
//! forks the standbys of execs later, so starts no thread). Its command and its execs run
//! under a filter that notifies of each (shards_seccomp::BINDS, `seccomp-binds=`), whose
//! listener each sends init as it starts, and init this process.
//!
//! A bind of a TCP or UDP socket of the run's network namespace to an address of a kind the
//! host has (its every address, its loopback, or the guest's own address on eth0, which is
//! the host's every address) is asked of the host first, which binds the same port there:
//! so a port the host holds fails it as in the host's own network namespace (EADDRINUSE),
//! and port 0 is one free on the host. This process then binds the socket itself, with the
//! address it read: what a notification shows can change before the call runs
//! (seccomp_unotify(2), its TOCTOU), and so what it bound is what the host bound. A TCP
//! socket's listen(2) has the host's listen, once the guest's socket listens, so that none
//! of the host's connections finds it not yet listening; a UDP socket's datagrams come as
//! it is bound. What comes to the host's port goes to the guest (its network process): to
//! the socket's own port on eth0's address where the socket takes that address; else (a
//! socket on the guest's loopback, or on IPv6 alone, which eth0 has none of) to a relay of
//! this process's there, at a port the host holds too, which passes each connection or
//! datagram on from the guest's own loopback, as a client on the host's loopback reaches a
//! server on it. A server's port stays the guest's own on its loopback
//! (net::hold_loopback_port), whose other ports are the host's (net::host_loopback): a TCP
//! socket's once it listens, a UDP socket's once it is bound to a port it named, or to the
//! loopback; a client's, bound to a port of the kernel's choosing, as musl's resolver binds
//! one, stays the host's, so that what it sends the host's loopback reaches it. As a socket
//! goes, which the kernel tells sock_diag's listeners (SKNLGRP_*_DESTROY), the host lets
//! its port go.
//!
//! This is no boundary (§9.9): a run on the host's network reaches what the host reaches,
//! and asked to.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use shards_abi::run::{self, HostBind, HostBound, HostListen, kind};

/// seccomp_data's arches (linux/audit.h), each with its own numbers for bind(2), listen(2)
/// and socketcall(2) (arch/x86/entry/syscalls, arch/arm/tools/syscall.tbl,
/// include/uapi/asm-generic/unistd.h); x32's are x86_64's with bit 30.
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
const AUDIT_ARCH_ARM: u32 = 0x4000_0028;
const X32: i32 = 0x4000_0000;
/// socketcall(2)'s calls (linux/net.h), its arguments 32-bit words in the caller's memory.
const SYS_BIND: u64 = 2;
const SYS_LISTEN: u64 = 4;
/// sock_diag (linux/netlink.h, linux/sock_diag.h, linux/inet_diag.h): its netlink family;
/// its requests' and messages' type; the groups of each TCP and UDP socket's end,
/// SKNLGRP_INET_TCP_DESTROY to SKNLGRP_INET6_UDP_DESTROY, bits 0 to 3 of a socket's groups.
const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const DESTROY_GROUPS: u32 = 0b1111;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
/// Where an inet_diag_msg's socket's cookie is, after its nlmsghdr (16 bytes): its family,
/// state, timer and retransmits (4), then its inet_diag_sockid, whose cookie, two u32s,
/// follows its ports (4), addresses (32) and interface (4).
const COOKIE_AT: usize = 16 + 4 + 40;
/// How many ports of the host's choosing a bind of port 0 tries where the guest has one of
/// them in use already.
const ANY_PORT_TRIES: usize = 8;
/// What one turn takes of one relay's connections or datagrams before the others have
/// theirs: Linux's own budget for a device per poll (net.core.dev_weight, 64;
/// Documentation/admin-guide/sysctl/net.rst), as the network process's.
const BUDGET: usize = 64;
/// How long a relayed UDP flow lives, as Linux conntrack keeps one, and the network
/// process (nf_conntrack_proto_udp.c, udp_packet; v6.12): 30 s past its last datagram, or
/// 120 s once it has had a reply and is still going 2 s after its first.
const UDP_UNREPLIED: Duration = Duration::from_secs(30);
const UDP_STREAM: Duration = Duration::from_secs(120);
const UDP_STREAM_AFTER: Duration = Duration::from_secs(2);
/// The largest UDP datagram (RFC 768: its length a 16-bit field).
const DATAGRAM: usize = 65_536;
/// How long the relays have, once the run has ended, to pass on what its servers sent
/// before they ended, as the kernel sends what a closed socket holds: the grace the run's
/// network process is given to end (shards netproc::GRACE).
const DRAIN: Duration = Duration::from_secs(1);
/// What each init's message to this process starts with: a listener, with it; or the
/// host's answer, after it.
pub const LISTENER: u8 = b'L';
const ANSWER: u8 = b'A';

/// Init's end of the process that makes the run's binds the host's, and its PID. Dropped,
/// it closes the process's end: the process takes no more, finishes its relays' connections
/// (at most [`DRAIN`]), and ends.
#[derive(Debug)]
pub struct Supervisor {
    sock: OwnedFd,
    pid: libc::pid_t,
}

/// Starts the process for a run on the host's network: in init, before the run's command
/// starts, so that its first bind is answered.
pub fn start() -> io::Result<Supervisor> {
    let (ours, theirs) = seqpacket()?;
    let diag = destroy_listener()?;
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            crate::dnsrelay::keep_only(&[theirs.as_raw_fd(), diag.as_raw_fd()]);
            serve(theirs, diag);
            // SAFETY: _exit(2) of the child, which owns nothing to flush.
            unsafe { libc::_exit(0) }
        }
        pid => Ok(Supervisor { sock: ours, pid }),
    }
}

impl Supervisor {
    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Gives the process a launched process's listener, the process container `owner`'s
    /// (0 the run's own, or a joiner's exec id, D119); false once it has gone.
    pub fn listener(&self, listener: &OwnedFd, owner: u32) -> bool {
        let [a, b, c, d] = owner.to_be_bytes();
        send_fd(
            self.sock.as_raw_fd(),
            &[LISTENER, a, b, c, d],
            listener.as_raw_fd(),
            libc::MSG_DONTWAIT,
        )
    }

    /// Gives the process the host's answer to one of its frames (BOUND's payload).
    pub fn answer(&self, bound: &[u8]) {
        let mut message = Vec::with_capacity(1 + bound.len());
        message.push(ANSWER);
        message.extend_from_slice(bound);
        // SAFETY: send(2) of a buffer of ours, of its length.
        unsafe {
            libc::send(
                self.sock.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
    }

    /// Each frame for the host the process has sent, whole, as `each` takes it; false once
    /// it has gone. One longer than any it sends is none of its, and skipped, rather than
    /// cut short in the host's stream (MSG_TRUNC says a message's whole length).
    pub fn frames(&self, mut each: impl FnMut(&[u8])) -> bool {
        let mut buf = [0u8; 256];
        loop {
            // SAFETY: recv(2) into a buffer of ours, of its length.
            let n = unsafe {
                libc::recv(
                    self.sock.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT | libc::MSG_TRUNC,
                )
            };
            match usize::try_from(n) {
                Ok(0) => return false,
                Ok(n) => {
                    if let Some(frame) = buf.get(..n) {
                        each(frame);
                    }
                }
                Err(_) => return io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock,
            }
        }
    }
}

/// A socket pair whose messages keep their bounds, closed on exec.
pub fn seqpacket() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: socketpair(2) fills two descriptors.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let [a, b] = fds;
    // SAFETY: fresh descriptors nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(a), OwnedFd::from_raw_fd(b)) })
}

/// Sends `message`, with descriptor `fd` (SCM_RIGHTS), on `channel`: whether all of it
/// went. Async-signal-safe calls alone, for the child of init's fork too.
pub fn send_fd(channel: RawFd, message: &[u8], fd: RawFd, flags: libc::c_int) -> bool {
    let mut iov = libc::iovec {
        iov_base: message.as_ptr().cast_mut().cast(),
        iov_len: message.len(),
    };
    // Room for one descriptor's control message, aligned as cmsghdr is.
    let mut control = [0u64; 4];
    // SAFETY: zeroed is a valid msghdr.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE of a descriptor's size.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as _;
    // SAFETY: the first header of a control buffer of ours, of the length set.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
    if cmsg.is_null() {
        return false;
    }
    // SAFETY: a header within the control buffer, and its data, of the length set.
    unsafe {
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
    }
    // SAFETY: sendmsg(2) of a message of ours, which it only reads.
    let sent = unsafe { libc::sendmsg(channel, &raw const msg, flags | libc::MSG_NOSIGNAL) };
    usize::try_from(sent).is_ok_and(|n| n == message.len())
}

/// One message from `channel` into `buf`, with the descriptor it carries, if any: its
/// length, 0 at its end, or the error.
pub fn recv_fd(channel: RawFd, buf: &mut [u8], flags: libc::c_int) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = [0u64; 4];
    // SAFETY: zeroed is a valid msghdr.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = size_of_val(&control) as _;
    // SAFETY: recvmsg(2) into buffers of ours, of the lengths set; the descriptors it
    // passes closed on exec.
    let n = unsafe { libc::recvmsg(channel, &raw mut msg, flags | libc::MSG_CMSG_CLOEXEC) };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    let mut fd = None;
    // SAFETY: the control buffer recvmsg filled, walked by its own macros.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
    while !cmsg.is_null() {
        // SAFETY: a header recvmsg wrote, within the control buffer.
        let (level, which, len) =
            unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len as usize) };
        if level == libc::SOL_SOCKET && which == libc::SCM_RIGHTS {
            // SAFETY: CMSG_LEN of no data, a constant.
            let head = unsafe { libc::CMSG_LEN(0) } as usize;
            let count = len.saturating_sub(head) / size_of::<RawFd>();
            for i in 0..count {
                // SAFETY: the i-th descriptor of the message's data, which holds `count`.
                let raw = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>().add(i)) };
                // SAFETY: a descriptor the kernel just installed for us alone; the first
                // kept, any other closed as it drops.
                let owned = unsafe { OwnedFd::from_raw_fd(raw) };
                if fd.is_none() {
                    fd = Some(owned);
                }
            }
        }
        // SAFETY: the next header after one within the buffer, or null.
        cmsg = unsafe { libc::CMSG_NXTHDR(&raw const msg, cmsg) };
    }
    Ok((n, fd))
}

/// A bind(2) or listen(2), with the arguments that matter here.
enum Call {
    Bind { fd: RawFd, addr: u64, len: i32 },
    Listen { fd: RawFd, backlog: u32 },
}

/// The call notification `n` is: bind(2) or listen(2), or, on a 32-bit ABI, socketcall(2)
/// for one, its arguments read from the caller's memory. Each argument is the C type the
/// kernel takes it as, an int its register's low 32 bits.
fn call(n: &libc::seccomp_notif) -> Option<Call> {
    let [a0, a1, a2, ..] = n.data.args;
    let int = |a: u64| a as u32 as i32;
    let which = match (n.data.arch, n.data.nr) {
        (AUDIT_ARCH_X86_64, 49)
        | (AUDIT_ARCH_I386, 361)
        | (AUDIT_ARCH_AARCH64, 200)
        | (AUDIT_ARCH_ARM, 282) => SYS_BIND,
        (AUDIT_ARCH_X86_64, nr) if nr == X32 | 49 => SYS_BIND,
        (AUDIT_ARCH_X86_64, 50)
        | (AUDIT_ARCH_I386, 363)
        | (AUDIT_ARCH_AARCH64, 201)
        | (AUDIT_ARCH_ARM, 284) => SYS_LISTEN,
        (AUDIT_ARCH_X86_64, nr) if nr == X32 | 50 => SYS_LISTEN,
        (AUDIT_ARCH_I386 | AUDIT_ARCH_ARM, 102) => {
            let pid = libc::pid_t::try_from(n.pid).ok()?;
            let used = match a0 {
                SYS_BIND => 12,
                SYS_LISTEN => 8,
                _ => return None,
            };
            let mut words = [0u8; 12];
            read_memory(pid, a1, words.get_mut(..used)?)?;
            let [w0, w1, w2] = words.as_chunks::<4>().0 else {
                return None;
            };
            let (w0, w1, w2) = (
                u32::from_ne_bytes(*w0),
                u32::from_ne_bytes(*w1),
                u32::from_ne_bytes(*w2),
            );
            return Some(if a0 == SYS_BIND {
                Call::Bind {
                    fd: w0 as i32,
                    addr: u64::from(w1),
                    len: w2 as i32,
                }
            } else {
                Call::Listen {
                    fd: w0 as i32,
                    backlog: w1,
                }
            });
        }
        _ => return None,
    };
    Some(if which == SYS_BIND {
        Call::Bind {
            fd: int(a0),
            addr: a1,
            len: int(a2),
        }
    } else {
        Call::Listen {
            fd: int(a0),
            backlog: a1 as u32,
        }
    })
}

/// Where a bind of the guest's goes on the host, and how what comes to it there comes to
/// the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapping {
    host: IpAddr,
    /// To the socket's own port on eth0's address, which it takes; else relayed to
    /// `target`, its own address, from the guest's loopback.
    direct: bool,
    target: IpAddr,
    /// Where the socket takes its port on the guest's loopback, which stays its own once it
    /// serves there: one address of 127/8, or every one (0.0.0.0); and whether it takes it
    /// on IPv6's, `::1`.
    hold: Option<Ipv4Addr>,
    hold6: bool,
}

/// The host's address for a bind of the guest's to `guest` (`v6only`, its IPV6_V6ONLY;
/// `own` and `own6`, eth0's addresses): the host's every address for the guest's every
/// address or eth0's own, its loopback address for the guest's. None for any other, which
/// the guest keeps (the kernel's to bind as asked).
fn mapping(guest: IpAddr, v6only: bool, own: Option<Ipv4Addr>, own6: Option<Ipv6Addr>) -> Option<Mapping> {
    let v4 = |a: Ipv4Addr| {
        if a.is_unspecified() {
            Some(Mapping {
                host: IpAddr::V4(a),
                direct: true,
                target: guest,
                hold: Some(a),
                hold6: false,
            })
        } else if a.is_loopback() {
            Some(Mapping {
                host: IpAddr::V4(a),
                direct: false,
                target: guest,
                hold: Some(a),
                hold6: false,
            })
        } else if Some(a) == own {
            Some(Mapping {
                host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                direct: true,
                target: guest,
                hold: None,
                hold6: false,
            })
        } else {
            None
        }
    };
    match guest {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(m) => v4(m),
            // Its every address: IPv4's too, but for IPv6's alone, which reaches the guest
            // at eth0's IPv4 address no more, so relayed to its loopback.
            None if a.is_unspecified() => Some(Mapping {
                host: guest,
                direct: !v6only,
                target: if v6only {
                    IpAddr::V6(Ipv6Addr::LOCALHOST)
                } else {
                    guest
                },
                hold: (!v6only).then_some(Ipv4Addr::UNSPECIFIED),
                hold6: true,
            }),
            None if a.is_loopback() => Some(Mapping {
                host: guest,
                direct: false,
                target: guest,
                hold: None,
                hold6: true,
            }),
            // eth0's own IPv6 address: the host's every IPv6 address, relayed to it.
            None if Some(a) == own6 => Some(Mapping {
                host: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                direct: false,
                target: guest,
                hold: None,
                hold6: false,
            }),
            None => None,
        },
    }
}

/// A notification's answer: the call left to the kernel, failed with an errno, done, or
/// answered later, once the host has; or none, its caller gone.
enum Answer {
    Continue,
    Errno(i32),
    Done,
    Later,
    Gone,
}

/// A notification waiting for the host's answer, by its listener's slot and its id; none
/// for a step taken after it was answered.
type Notif = Option<(usize, u64)>;

struct Pending {
    notif: Notif,
    step: Step,
}

enum Step {
    /// A bind of the guest's `sock` to `guest`, the host binding `ask` for it; listening
    /// with `listen` once bound, where it was a listen of a socket not yet bound.
    Bind(Bind),
    /// The host's listen for held socket `cookie`, the guest's `sock` listening.
    Listen { sock: OwnedFd, cookie: u64 },
    /// A port the host holds for held socket `cookie`'s relay, then the guest's `sock`'s
    /// listen with its backlog (TCP), or its datagrams' coming (UDP).
    Relay {
        cookie: u64,
        tries: usize,
        listen: Option<(OwnedFd, u32)>,
    },
    /// Held UDP socket `cookie`'s datagrams coming to it.
    Publish { cookie: u64 },
}

/// A bind of the guest's `sock` to `guest`, the host binding `ask` for it, `tries` more
/// times where the guest has the port the host chose; listening with `listen` once bound,
/// where it was a listen of a socket not yet bound; asked again once already, where the
/// host had the port for a socket of the guest's that had gone.
struct Bind {
    sock: OwnedFd,
    guest: IpAddr,
    ask: HostBind,
    m: Mapping,
    tries: usize,
    listen: Option<u32>,
    again: bool,
}

/// A socket of the guest's bound on the host, by its cookie: the host's number for it.
struct Held {
    number: u32,
    proto: u8,
    port: u16,
    m: Mapping,
    /// The host listens for it, or passes its datagrams on.
    listening: bool,
    /// The host's listen for it is asked.
    asked: bool,
    relay: Option<Relay>,
    /// It keeps its port on the guest's loopback, a server's there.
    kept: bool,
    /// The container whose process bound it ([`HostBind::owner`]).
    owner: u32,
}

/// A relay to a held socket that eth0's address does not reach: its socket there, at a
/// port the host holds too, by its number; where what comes to it goes.
struct Relay {
    sock: OwnedFd,
    number: u32,
    target: SocketAddr,
    udp: bool,
}

/// One way of a relayed connection: what was read and not yet written.
struct Half {
    buf: Box<[u8]>,
    start: usize,
    end: usize,
    /// What it reads from has ended; and, all written, so has what it writes to.
    eof: bool,
    shut: bool,
}

impl Half {
    fn new() -> Half {
        Half {
            buf: vec![0; run::CHUNK].into_boxed_slice(),
            start: 0,
            end: 0,
            eof: false,
            shut: false,
        }
    }

    fn has_room(&self) -> bool {
        !self.eof && (self.start == self.end || self.end < self.buf.len())
    }

    fn has_data(&self) -> bool {
        self.start < self.end
    }

    /// Reads what `from` has; false on its error.
    fn fill(&mut self, from: RawFd) -> bool {
        if self.start == self.end {
            (self.start, self.end) = (0, 0);
        }
        if self.eof {
            return true;
        }
        let Some(room) = self.buf.get_mut(self.end..).filter(|r| !r.is_empty()) else {
            return true;
        };
        // SAFETY: recv(2) into a buffer of ours, of its length.
        let n = unsafe { libc::recv(from, room.as_mut_ptr().cast(), room.len(), libc::MSG_DONTWAIT) };
        match usize::try_from(n) {
            Ok(0) => self.eof = true,
            Ok(n) => self.end += n,
            Err(_) => return io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock,
        }
        true
    }

    /// Writes what it holds to `to`, and its end once all is written; false on an error.
    fn drain(&mut self, to: RawFd) -> bool {
        if let Some(data) = self.buf.get(self.start..self.end).filter(|d| !d.is_empty()) {
            // SAFETY: send(2) of a buffer of ours, of its length.
            let n = unsafe {
                libc::send(
                    to,
                    data.as_ptr().cast(),
                    data.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            match usize::try_from(n) {
                Ok(n) => self.start += n,
                Err(_) => return io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock,
            }
        }
        if self.eof && !self.has_data() && !self.shut {
            // SAFETY: shutdown(2) of a socket of ours.
            unsafe { libc::shutdown(to, libc::SHUT_WR) };
            self.shut = true;
        }
        true
    }
}

/// A relayed connection: the network process's, and this process's to the socket it
/// reaches, connecting until `connected`.
struct Pair {
    net: OwnedFd,
    local: OwnedFd,
    connected: bool,
    up: Half,
    down: Half,
}

/// A relayed UDP flow: the network process's peer, from relay `relay` (a held socket's
/// cookie), and this process's socket to the guest's, connected to it.
struct Flow {
    sock: OwnedFd,
    relay: u64,
    peer: SocketAddr,
    first: Instant,
    replied: bool,
    until: Instant,
}

impl Flow {
    /// A datagram at `now`; a `reply` if it came from the guest's socket.
    fn datagram(&mut self, now: Instant, reply: bool) {
        self.replied |= reply;
        let stream = self.replied && now > self.first + UDP_STREAM_AFTER;
        self.until = now + if stream { UDP_STREAM } else { UDP_UNREPLIED };
    }
}

#[derive(Clone, Copy)]
enum Owner {
    Ctl,
    Diag,
    Listener(usize),
    Relay(u64),
    Pair(usize),
    Flow(usize),
}

struct Supervise {
    ctl: OwnedFd,
    diag: OwnedFd,
    netns: Option<u64>,
    own: Option<Ipv4Addr>,
    /// eth0's IPv6 address and its gateway's, where it has IPv6.
    own6: Option<(Ipv6Addr, Ipv6Addr)>,
    /// Each launched process's listener, and the container whose process it is.
    listeners: Vec<Option<(OwnedFd, u32)>>,
    pending: HashMap<u32, Pending>,
    next: u32,
    held: HashMap<u64, Held>,
    /// How many held sockets keep each port of the guest's loopback, by protocol, address
    /// and port; and of IPv6's, by protocol and port.
    holds: HashMap<(u8, Ipv4Addr, u16), u32>,
    holds6: HashMap<(u8, u16), u32>,
    /// The host's ports let go, by protocol and port: the number the next request had
    /// then.
    freed: HashMap<(u8, u16), u32>,
    pairs: Vec<Option<Pair>>,
    flows: Vec<Option<Flow>>,
    by_peer: HashMap<(u64, SocketAddr), usize>,
    datagram: Vec<u8>,
}

/// The process's loop, until init closes its end.
fn serve(ctl: OwnedFd, diag: OwnedFd) {
    let mut s = Supervise {
        ctl,
        diag,
        netns: own_netns(),
        own: crate::net::current().map(|(a, _, _)| a),
        own6: crate::net::current6().map(|(a, _, gateway)| (a, gateway)),
        listeners: Vec::new(),
        pending: HashMap::new(),
        next: 0,
        held: HashMap::new(),
        holds: HashMap::new(),
        holds6: HashMap::new(),
        freed: HashMap::new(),
        pairs: Vec::new(),
        flows: Vec::new(),
        by_peer: HashMap::new(),
        datagram: vec![0; DATAGRAM],
    };
    let mut watched: Vec<(RawFd, libc::c_short, Owner)> = Vec::new();
    let mut set: Vec<libc::pollfd> = Vec::new();
    // Init gone, the run has ended: what its relays hold, passed on until this.
    let mut until: Option<Instant> = None;
    loop {
        if let Some(until) = until
            && (s.pairs.iter().all(Option::is_none) || Instant::now() >= until)
        {
            return;
        }
        watched.clear();
        if until.is_none() {
            watched.push((s.ctl.as_raw_fd(), libc::POLLIN, Owner::Ctl));
        }
        watched.push((s.diag.as_raw_fd(), libc::POLLIN, Owner::Diag));
        for (i, l) in s.listeners.iter().enumerate() {
            if let Some((l, _)) = l {
                watched.push((l.as_raw_fd(), libc::POLLIN, Owner::Listener(i)));
            }
        }
        for (&cookie, h) in &s.held {
            if let Some(r) = &h.relay {
                watched.push((r.sock.as_raw_fd(), libc::POLLIN, Owner::Relay(cookie)));
            }
        }
        for (i, p) in s.pairs.iter().enumerate() {
            let Some(p) = p else { continue };
            let net = if p.up.has_room() { libc::POLLIN } else { 0 }
                | if p.down.has_data() { libc::POLLOUT } else { 0 };
            let local = if !p.connected || p.up.has_data() {
                libc::POLLOUT
            } else {
                0
            } | if p.connected && p.down.has_room() {
                libc::POLLIN
            } else {
                0
            };
            watched.push((p.net.as_raw_fd(), net, Owner::Pair(i)));
            watched.push((p.local.as_raw_fd(), local, Owner::Pair(i)));
        }
        for (i, f) in s.flows.iter().enumerate() {
            if let Some(f) = f {
                watched.push((f.sock.as_raw_fd(), libc::POLLIN, Owner::Flow(i)));
            }
        }
        set.clear();
        set.extend(watched.iter().map(|&(fd, events, _)| libc::pollfd {
            fd,
            events,
            revents: 0,
        }));
        let now = Instant::now();
        let timeout = s
            .flows
            .iter()
            .flatten()
            .map(|f| f.until)
            .chain(until)
            .map(|t| t.saturating_duration_since(now))
            .min()
            .map_or(-1, |d| {
                libc::c_int::try_from(d.as_millis().saturating_add(1)).unwrap_or(libc::c_int::MAX)
            });
        // SAFETY: poll(2) on pollfds of descriptors this process holds, as many as given.
        if unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, timeout) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        for (p, &(_, _, owner)) in set.iter().zip(&watched).filter(|(p, _)| p.revents != 0) {
            match owner {
                Owner::Ctl => {
                    if !s.heard() {
                        // No more binds, connections nor datagrams: the run has ended.
                        // What its servers answered its relays' peers before they ended is
                        // passed on first, as the kernel sends what a socket was given.
                        until = Some(Instant::now() + DRAIN);
                        s.listeners.clear();
                        for i in 0..s.flows.len() {
                            s.replied(i);
                        }
                        s.flows.clear();
                        s.by_peer.clear();
                        for h in s.held.values_mut() {
                            h.relay = None;
                        }
                    }
                }
                Owner::Diag => s.destroyed(),
                Owner::Listener(i) => {
                    if p.revents & libc::POLLIN != 0 {
                        s.notified(i);
                    } else if let Some(slot) = s.listeners.get_mut(i) {
                        // Its processes are gone.
                        *slot = None;
                    }
                }
                Owner::Relay(cookie) => s.relayed(cookie),
                Owner::Pair(i) => s.pump(i),
                Owner::Flow(i) => s.replied(i),
            }
        }
        s.expire(Instant::now());
    }
}

impl Supervise {
    /// What init has sent: listeners and the host's answers; false once init has gone.
    fn heard(&mut self) -> bool {
        let mut buf = [0u8; 64];
        loop {
            match recv_fd(self.ctl.as_raw_fd(), &mut buf, libc::MSG_DONTWAIT) {
                Ok((0, _)) => return false,
                Ok((n, fd)) => match (buf.get(..n), fd) {
                    (Some(&[LISTENER, a, b, c, d]), Some(fd)) => {
                        self.listeners.push(Some((fd, u32::from_be_bytes([a, b, c, d]))));
                    }
                    (Some([ANSWER, bound @ ..]), _) => {
                        if let Some(b) = HostBound::decode(bound) {
                            self.answered(b);
                        }
                    }
                    _ => {}
                },
                Err(e) => return e.kind() == io::ErrorKind::WouldBlock,
            }
        }
    }

    /// Sends the host frame `which` with `payload`, by init.
    fn tell(&self, which: u8, payload: &[u8]) -> bool {
        let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        let mut frame = run::header(which, len).to_vec();
        frame.extend_from_slice(payload);
        // SAFETY: send(2) of a buffer of ours, of its length.
        let n = unsafe {
            libc::send(
                self.ctl.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        usize::try_from(n).is_ok_and(|n| n == frame.len())
    }

    /// Asks the host for `step`'s frame, which `frame` makes of its request's number: the
    /// notification answered later, or at once where the host cannot be asked.
    fn ask(&mut self, notif: Notif, step: Step, which: u8, frame: impl FnOnce(u32) -> Vec<u8>) -> Answer {
        let request = self.next;
        self.next = self.next.wrapping_add(1);
        if !self.tell(which, &frame(request)) {
            return Answer::Errno(libc::ENETDOWN);
        }
        self.pending.insert(request, Pending { notif, step });
        Answer::Later
    }

    fn unbind(&self, number: u32) {
        self.tell(kind::UNBIND, &number.to_be_bytes());
    }

    fn respond(&self, notif: Notif, answer: Answer) {
        let Some((slot, id)) = notif else { return };
        let Some((listener, _)) = self.listeners.get(slot).and_then(Option::as_ref) else {
            return;
        };
        let mut resp = libc::seccomp_notif_resp {
            id,
            val: 0,
            error: 0,
            flags: 0,
        };
        match answer {
            Answer::Continue => resp.flags = libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
            Answer::Done => {}
            Answer::Errno(e) => resp.error = -e,
            Answer::Later | Answer::Gone => return,
        }
        // SAFETY: ioctl(2) sending a struct of the size its number says.
        unsafe {
            libc::ioctl(
                listener.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_SEND,
                &raw const resp,
            )
        };
    }

    /// One notification of listener `slot`'s, answered, or to be once the host has.
    fn notified(&mut self, slot: usize) {
        let Some((listener, owner)) = self
            .listeners
            .get(slot)
            .and_then(Option::as_ref)
            .map(|(l, owner)| (l.as_raw_fd(), *owner))
        else {
            return;
        };
        // SAFETY: zeroed is a valid seccomp_notif; the kernel fills it.
        let mut n: libc::seccomp_notif = unsafe { std::mem::zeroed() };
        // SAFETY: ioctl(2) receiving into a struct of the size its number says.
        if unsafe { libc::ioctl(listener, libc::SECCOMP_IOCTL_NOTIF_RECV, &raw mut n) } != 0 {
            return;
        }
        let notif = Some((slot, n.id));
        let answer = self.take(listener, notif, &n, owner);
        self.respond(notif, answer);
    }

    /// Notification `n` of a process of container `owner`'s.
    fn take(&mut self, listener: RawFd, notif: Notif, n: &libc::seccomp_notif, owner: u32) -> Answer {
        let Some(call) = call(n) else {
            return Answer::Continue;
        };
        let Ok(pid) = libc::pid_t::try_from(n.pid) else {
            return Answer::Continue;
        };
        let (Call::Bind { fd, .. } | Call::Listen { fd, .. }) = call;
        // A TCP or UDP socket of the run's own network namespace, or the kernel's.
        let Some(sock) = socket_of(pid, fd) else {
            return Answer::Continue;
        };
        let Some((domain, proto)) = inet(&sock) else {
            return Answer::Continue;
        };
        if self.netns.is_none() || netns_cookie(&sock) != self.netns {
            return Answer::Continue;
        }
        let v6only = domain == libc::AF_INET6
            && sockopt(&sock, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY).is_some_and(|v| v != 0);
        match call {
            Call::Bind { addr, len, .. } => {
                // Bound already: the kernel's EINVAL.
                if local_port(&sock) != Some(0) {
                    return Answer::Continue;
                }
                let Some((guest, port)) = read_sockaddr(pid, addr, len, domain) else {
                    return Answer::Continue;
                };
                // What it asked, read while it still asks (seccomp_unotify(2)).
                if !valid(listener, n.id) {
                    return Answer::Gone;
                }
                let Some(m) = mapping(guest, v6only, self.own, self.own6.map(|(a, _)| a)) else {
                    return Answer::Continue;
                };
                self.bind(notif, sock, (guest, port), proto, m, None, owner)
            }
            Call::Listen { backlog, .. } => {
                if proto != libc::IPPROTO_TCP as u8 {
                    return Answer::Continue;
                }
                match local_port(&sock) {
                    // Not yet bound: bound by listen(2) to its family's every address,
                    // at a port of the host's choosing (inet_autobind).
                    Some(0) => {
                        let any = if domain == libc::AF_INET6 {
                            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
                        } else {
                            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                        };
                        match mapping(any, v6only, self.own, self.own6.map(|(a, _)| a)) {
                            Some(m) => self.bind(notif, sock, (any, 0), proto, m, Some(backlog), owner),
                            None => Answer::Continue,
                        }
                    }
                    Some(_) => self.listen(notif, sock, backlog),
                    None => Answer::Continue,
                }
            }
        }
    }

    /// Asks the host to bind `port` for the guest's `sock`, of protocol `proto`, a process
    /// of container `owner`'s, to bind it to `guest` once the host has.
    #[allow(clippy::too_many_arguments)]
    fn bind(
        &mut self,
        notif: Notif,
        sock: OwnedFd,
        (guest, port): (IpAddr, u16),
        proto: u8,
        m: Mapping,
        listen: Option<u32>,
        owner: u32,
    ) -> Answer {
        let flag = |level, name| sockopt(&sock, level, name).is_some_and(|v| v != 0);
        let ask = HostBind {
            request: 0,
            owner,
            proto,
            addr: m.host,
            port,
            reuse_addr: flag(libc::SOL_SOCKET, libc::SO_REUSEADDR),
            reuse_port: flag(libc::SOL_SOCKET, libc::SO_REUSEPORT),
            v6only: m.host.is_ipv6() && flag(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY),
        };
        let tries = if port == 0 { ANY_PORT_TRIES } else { 1 };
        self.ask_bind(
            notif,
            Bind {
                sock,
                guest,
                ask,
                m,
                tries,
                listen,
                again: false,
            },
        )
    }

    fn ask_bind(&mut self, notif: Notif, b: Bind) -> Answer {
        let ask = b.ask;
        self.ask(notif, Step::Bind(b), kind::BIND, |request| {
            HostBind { request, ..ask }.encode()
        })
    }

    /// The listen of the guest's `sock`, bound for the host: the guest's socket listening,
    /// then the host's.
    fn listen(&mut self, notif: Notif, sock: OwnedFd, backlog: u32) -> Answer {
        let Some(cookie) = cookie(&sock) else {
            return Answer::Continue;
        };
        let Some(h) = self.held.get_mut(&cookie) else {
            return Answer::Continue;
        };
        // The guest's first, so that none of the host's connections finds it not yet
        // listening.
        // SAFETY: listen(2) of a socket of ours, the guest's.
        if unsafe { libc::listen(sock.as_raw_fd(), backlog as libc::c_int) } != 0 {
            return Answer::Errno(errno());
        }
        // A listen again (its backlog), or one already asked.
        if h.listening || h.asked {
            return Answer::Done;
        }
        h.asked = true;
        let (number, port, direct) = (h.number, h.port, h.m.direct);
        // A server's: its port its own on the guest's loopback, before its relay reaches
        // it there.
        self.keep(cookie);
        if direct {
            let step = Step::Listen { sock, cookie };
            self.ask(notif, step, kind::LISTEN, |request| {
                HostListen {
                    request,
                    number,
                    backlog,
                    guest_port: port,
                }
                .encode()
                .to_vec()
            })
        } else {
            self.ask_relay(notif, cookie, ANY_PORT_TRIES, Some((sock, backlog)))
        }
    }

    /// Asks the host for a port for held socket `cookie`'s relay.
    fn ask_relay(
        &mut self,
        notif: Notif,
        cookie: u64,
        tries: usize,
        listen: Option<(OwnedFd, u32)>,
    ) -> Answer {
        let Some((proto, owner)) = self.held.get(&cookie).map(|h| (h.proto, h.owner)) else {
            return Answer::Errno(libc::EBADF);
        };
        let ask = HostBind {
            request: 0,
            owner,
            proto,
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            reuse_addr: false,
            reuse_port: false,
            v6only: false,
        };
        let step = Step::Relay {
            cookie,
            tries,
            listen,
        };
        self.ask(notif, step, kind::BIND, |request| {
            HostBind { request, ..ask }.encode()
        })
    }

    /// The host's answer to a frame of this process's.
    fn answered(&mut self, bound: HostBound) {
        let Some(Pending { notif, step }) = self.pending.remove(&bound.request) else {
            return;
        };
        let answer = match step {
            Step::Bind(b) => self.bound(notif, bound, b),
            Step::Listen { sock, cookie } => {
                if let Some(h) = self.held.get_mut(&cookie) {
                    h.asked = false;
                    h.listening = bound.errno == 0;
                }
                if bound.errno == 0 {
                    Answer::Done
                } else {
                    unlisten(&sock);
                    Answer::Errno(bound.errno)
                }
            }
            Step::Relay {
                cookie,
                tries,
                listen,
            } => self.relay_bound(notif, bound, cookie, tries, listen),
            Step::Publish { cookie } => {
                if let Some(h) = self.held.get_mut(&cookie) {
                    h.asked = false;
                    h.listening = bound.errno == 0;
                }
                Answer::Gone
            }
        };
        self.respond(notif, answer);
    }

    /// The host's answer to bind `b`: the guest's socket bound as the host's was, and
    /// held; or the host's errno; or, where the guest has the port the host chose,
    /// another.
    fn bound(&mut self, notif: Notif, bound: HostBound, b: Bind) -> Answer {
        // The host's port held still for a socket of the guest's that has gone, whose end
        // the kernel tells after the close (sock_diag's broadcast is queued work): a port
        // closed and bound again at once is the guest's to bind, as the host's own would
        // be. Asked again, once each such socket is let go, by a frame before the bind's:
        // one gone now, or let go since this bind was asked.
        let key = (b.ask.proto, b.ask.port);
        if bound.errno == libc::EADDRINUSE
            && b.ask.port != 0
            && !b.again
            && (self
                .freed
                .get(&key)
                .is_some_and(|&at| at.wrapping_sub(bound.request) as i32 > 0)
                || self.gone(key.0, key.1))
        {
            return self.ask_bind(notif, Bind { again: true, ..b });
        }
        if bound.errno != 0 {
            return Answer::Errno(bound.errno);
        }
        match bind_socket(&b.sock, b.guest, bound.port) {
            Err(e) if e == libc::EADDRINUSE && b.ask.port == 0 && b.tries > 1 => {
                self.unbind(bound.number);
                self.ask_bind(
                    notif,
                    Bind {
                        tries: b.tries - 1,
                        ..b
                    },
                )
            }
            Err(e) => {
                self.unbind(bound.number);
                Answer::Errno(e)
            }
            Ok(()) => {
                let Some(cookie) = cookie(&b.sock) else {
                    // Kept by the host until the run ends.
                    return Answer::Done;
                };
                self.held.insert(
                    cookie,
                    Held {
                        number: bound.number,
                        proto: b.ask.proto,
                        port: bound.port,
                        m: b.m,
                        listening: false,
                        asked: false,
                        relay: None,
                        kept: false,
                        owner: b.ask.owner,
                    },
                );
                // A UDP socket's datagrams come as it is bound: answered at once, the
                // host's socket keeping what comes meanwhile. A server's port, one it
                // named or on the loopback alone, is its own there, which its relay
                // reaches it by.
                if b.ask.proto == libc::IPPROTO_UDP as u8 {
                    if b.ask.port != 0 || !b.m.direct {
                        self.keep(cookie);
                    }
                    self.publish(cookie);
                }
                match b.listen {
                    Some(backlog) => self.listen(notif, b.sock, backlog),
                    None => Answer::Done,
                }
            }
        }
    }

    /// Held UDP socket `cookie`'s datagrams to come to it: to its port, or to a relay.
    fn publish(&mut self, cookie: u64) {
        let Some(h) = self.held.get_mut(&cookie) else {
            return;
        };
        h.asked = true;
        let (number, port) = (h.number, h.port);
        if h.m.direct {
            let step = Step::Publish { cookie };
            self.ask(None, step, kind::LISTEN, |request| {
                HostListen {
                    request,
                    number,
                    backlog: 0,
                    guest_port: port,
                }
                .encode()
                .to_vec()
            });
        } else {
            self.ask_relay(None, cookie, ANY_PORT_TRIES, None);
        }
    }

    /// The host's port for held socket `cookie`'s relay: the relay made there, then the
    /// host's listen for the socket, with the relay's port as the guest's.
    fn relay_bound(
        &mut self,
        notif: Notif,
        bound: HostBound,
        cookie: u64,
        tries: usize,
        listen: Option<(OwnedFd, u32)>,
    ) -> Answer {
        let failed = |listen: Option<(OwnedFd, u32)>, errno: i32| match listen {
            Some((sock, _)) => {
                unlisten(&sock);
                Answer::Errno(errno)
            }
            None => Answer::Gone,
        };
        let Some(h) = self.held.get_mut(&cookie) else {
            if bound.errno == 0 {
                self.unbind(bound.number);
            }
            return failed(listen, libc::EBADF);
        };
        if bound.errno != 0 {
            h.asked = false;
            return failed(listen, bound.errno);
        }
        let udp = h.proto == libc::IPPROTO_UDP as u8;
        let target = SocketAddr::new(h.m.target, h.port);
        let number = h.number;
        let made = self
            .own
            .ok_or(libc::EADDRNOTAVAIL)
            .and_then(|own| relay_socket(own, bound.port, udp));
        let sock = match made {
            Ok(sock) => sock,
            Err(e) => {
                self.unbind(bound.number);
                if e == libc::EADDRINUSE && tries > 1 {
                    return self.ask_relay(notif, cookie, tries - 1, listen);
                }
                if let Some(h) = self.held.get_mut(&cookie) {
                    h.asked = false;
                }
                return failed(listen, e);
            }
        };
        if let Some(h) = self.held.get_mut(&cookie) {
            h.relay = Some(Relay {
                sock,
                number: bound.number,
                target,
                udp,
            });
        }
        let relay_port = bound.port;
        let (backlog, step) = match listen {
            Some((sock, backlog)) => (backlog, Step::Listen { sock, cookie }),
            None => (0, Step::Publish { cookie }),
        };
        self.ask(notif, step, kind::LISTEN, |request| {
            HostListen {
                request,
                number,
                backlog,
                guest_port: relay_port,
            }
            .encode()
            .to_vec()
        })
    }

    /// Held socket `cookie`, a server, keeps its port on the guest's loopback, where it
    /// takes it.
    fn keep(&mut self, cookie: u64) {
        let Some(h) = self.held.get_mut(&cookie).filter(|h| !h.kept) else {
            return;
        };
        h.kept = true;
        let (proto, port, hold, hold6) = (h.proto, h.port, h.m.hold, h.m.hold6);
        if let Some(addr) = hold {
            let count = self.holds.entry((proto, addr, port)).or_insert(0);
            if *count == 0 {
                let _ = crate::net::hold_loopback_port(proto, addr, port, true);
            }
            *count += 1;
        }
        if hold6 {
            let count = self.holds6.entry((proto, port)).or_insert(0);
            *count += 1;
            if *count == 1 {
                self.held6_changed();
            }
        }
    }

    /// IPv6's loopback's ports the run's servers hold: its translation to the host's made
    /// again with them (links::host_loopback6).
    fn held6_changed(&self) {
        if let Some((guest6, gateway6)) = self.own6 {
            let held: Vec<(u8, u16)> = self.holds6.keys().copied().collect();
            let _ = crate::links::host_loopback6(guest6, gateway6, &held);
        }
    }

    /// The sockets sock_diag says have gone: each held one's host port let go. Where the
    /// kernel had more to say than this socket could hold (ENOBUFS), every socket of the
    /// guest's is listed instead, and each held one not among them let go.
    fn destroyed(&mut self) {
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: recv(2) into a buffer of ours, nonblocking.
            let got = unsafe {
                libc::recv(
                    self.diag.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            let Ok(got) = usize::try_from(got) else {
                if errno() == libc::ENOBUFS {
                    self.resync();
                    continue;
                }
                return;
            };
            for cookie in cookies(buf.get(..got).unwrap_or_default()) {
                self.let_go(cookie);
            }
        }
    }

    /// Lets go of each held socket of `proto` at `port` the guest no longer has: whether
    /// there was one.
    fn gone(&mut self, proto: u8, port: u16) -> bool {
        let at: Vec<u64> = self
            .held
            .iter()
            .filter(|(_, h)| h.proto == proto && h.port == port)
            .map(|(&c, _)| c)
            .collect();
        if at.is_empty() {
            return false;
        }
        let Some(live) = sockets() else { return false };
        let mut any = false;
        for cookie in at.into_iter().filter(|c| !live.contains(c)) {
            self.let_go(cookie);
            any = true;
        }
        any
    }

    /// Lets go of each held socket the guest no longer has.
    fn resync(&mut self) {
        let Some(live) = sockets() else { return };
        let gone: Vec<u64> = self.held.keys().filter(|c| !live.contains(c)).copied().collect();
        for cookie in gone {
            self.let_go(cookie);
        }
    }

    fn let_go(&mut self, cookie: u64) {
        // Its relay's flows go with it, what they hold passed on first: a server's last
        // answers, sent before it closed its socket. Its connections, each a socket of its
        // own, go on.
        let flows: Vec<usize> = self
            .flows
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.as_ref().filter(|f| f.relay == cookie).map(|_| i))
            .collect();
        for &i in &flows {
            self.replied(i);
        }
        let Some(h) = self.held.remove(&cookie) else {
            return;
        };
        self.unbind(h.number);
        self.freed.insert((h.proto, h.port), self.next);
        if let Some(r) = h.relay {
            self.unbind(r.number);
            for i in flows {
                self.end_flow(i);
            }
        }
        if let Some(addr) = h.m.hold.filter(|_| h.kept)
            && let Some(count) = self.holds.get_mut(&(h.proto, addr, h.port))
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.holds.remove(&(h.proto, addr, h.port));
                let _ = crate::net::hold_loopback_port(h.proto, addr, h.port, false);
            }
        }
        if h.m.hold6
            && h.kept
            && let Some(count) = self.holds6.get_mut(&(h.proto, h.port))
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.holds6.remove(&(h.proto, h.port));
                self.held6_changed();
            }
        }
    }

    /// What relay `cookie` has: its connections, each to its socket, or its datagrams,
    /// each on its peer's flow.
    fn relayed(&mut self, cookie: u64) {
        let Some(r) = self.held.get(&cookie).and_then(|h| h.relay.as_ref()) else {
            return;
        };
        let (fd, target, udp) = (r.sock.as_raw_fd(), r.target, r.udp);
        for _ in 0..BUDGET {
            if udp {
                let Some((n, peer)) = recv_from(fd, &mut self.datagram) else {
                    return;
                };
                let now = Instant::now();
                let i = match self.by_peer.get(&(cookie, peer)) {
                    Some(&i) => i,
                    None => {
                        let Some(sock) = connected(target, libc::SOCK_DGRAM) else {
                            continue;
                        };
                        let flow = Flow {
                            sock,
                            relay: cookie,
                            peer,
                            first: now,
                            replied: false,
                            until: now + UDP_UNREPLIED,
                        };
                        let i = put(&mut self.flows, flow);
                        self.by_peer.insert((cookie, peer), i);
                        i
                    }
                };
                if let Some(f) = self.flows.get_mut(i).and_then(Option::as_mut) {
                    f.datagram(now, false);
                    let data = self.datagram.get(..n).unwrap_or_default();
                    // SAFETY: send(2) of a buffer of ours, of its length.
                    unsafe {
                        libc::send(
                            f.sock.as_raw_fd(),
                            data.as_ptr().cast(),
                            data.len(),
                            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                        )
                    };
                }
            } else {
                // SAFETY: accept4(2) of a listening socket of ours, no address asked for.
                let net = unsafe {
                    libc::accept4(
                        fd,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    )
                };
                if net < 0 {
                    return;
                }
                // SAFETY: a descriptor just made, owned here alone.
                let net = unsafe { OwnedFd::from_raw_fd(net) };
                let Some(local) = connected(target, libc::SOCK_STREAM) else {
                    continue;
                };
                put(
                    &mut self.pairs,
                    Pair {
                        net,
                        local,
                        connected: false,
                        up: Half::new(),
                        down: Half::new(),
                    },
                );
            }
        }
    }

    /// Moves what relayed connection `i` can, each way; ends it once both ways have ended,
    /// or on an error.
    fn pump(&mut self, i: usize) {
        let Some(p) = self.pairs.get_mut(i).and_then(Option::as_mut) else {
            return;
        };
        let (net, local) = (p.net.as_raw_fd(), p.local.as_raw_fd());
        if !p.connected {
            match sockopt_raw(local, libc::SOL_SOCKET, libc::SO_ERROR) {
                Some(0) if peer_known(local) => p.connected = true,
                Some(0) => return,
                _ => {
                    self.end_pair(i);
                    return;
                }
            }
        }
        let ok = p.up.fill(net) && p.up.drain(local) && p.down.fill(local) && p.down.drain(net);
        if !ok || (p.up.shut && p.down.shut) {
            self.end_pair(i);
        }
    }

    fn end_pair(&mut self, i: usize) {
        if let Some(slot) = self.pairs.get_mut(i) {
            *slot = None;
        }
    }

    /// The guest's socket's datagrams on flow `i`, back to its peer through its relay.
    fn replied(&mut self, i: usize) {
        let Some(f) = self.flows.get(i).and_then(Option::as_ref) else {
            return;
        };
        let (fd, peer, relay) = (f.sock.as_raw_fd(), f.peer, f.relay);
        let Some(out) = self
            .held
            .get(&relay)
            .and_then(|h| h.relay.as_ref())
            .map(|r| r.sock.as_raw_fd())
        else {
            self.end_flow(i);
            return;
        };
        for _ in 0..BUDGET {
            // SAFETY: recv(2) into a buffer of ours, nonblocking.
            let n = unsafe {
                libc::recv(
                    fd,
                    self.datagram.as_mut_ptr().cast(),
                    self.datagram.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            let Ok(n) = usize::try_from(n) else { return };
            send_to(out, self.datagram.get(..n).unwrap_or_default(), peer);
            if let Some(f) = self.flows.get_mut(i).and_then(Option::as_mut) {
                f.datagram(Instant::now(), true);
            }
        }
    }

    fn end_flow(&mut self, i: usize) {
        if let Some(f) = self.flows.get_mut(i).and_then(Option::take) {
            self.by_peer.remove(&(f.relay, f.peer));
        }
    }

    /// Ends the flows whose time is up.
    fn expire(&mut self, now: Instant) {
        let over: Vec<usize> = self
            .flows
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.as_ref().filter(|f| f.until <= now).map(|_| i))
            .collect();
        for i in over {
            self.end_flow(i);
        }
    }
}

/// `item` in the first free slot of `slots`, or a new one: its index.
fn put<T>(slots: &mut Vec<Option<T>>, item: T) -> usize {
    match slots.iter().position(Option::is_none) {
        Some(i) => {
            if let Some(slot) = slots.get_mut(i) {
                *slot = Some(item);
            }
            i
        }
        None => {
            slots.push(Some(item));
            slots.len() - 1
        }
    }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// Whether notification `id` still waits on `listener`: its caller has not gone, nor been
/// interrupted, so its PID and memory are still its.
fn valid(listener: RawFd, id: u64) -> bool {
    // SAFETY: ioctl(2) with a u64 of ours.
    unsafe { libc::ioctl(listener, libc::SECCOMP_IOCTL_NOTIF_ID_VALID, &raw const id) == 0 }
}

/// `pid`'s memory at `at` into `out`, whole or none.
fn read_memory(pid: libc::pid_t, at: u64, out: &mut [u8]) -> Option<()> {
    let local = libc::iovec {
        iov_base: out.as_mut_ptr().cast(),
        iov_len: out.len(),
    };
    let remote = libc::iovec {
        iov_base: usize::try_from(at).ok()? as *mut libc::c_void,
        iov_len: out.len(),
    };
    // SAFETY: process_vm_readv(2) into our own buffer of the length given.
    let n = unsafe { libc::process_vm_readv(pid, &raw const local, 1, &raw const remote, 1, 0) };
    (usize::try_from(n).ok()? == out.len()).then_some(())
}

/// The socket `fd` of process `pid`, as a descriptor of ours (pidfd_getfd(2)).
fn socket_of(pid: libc::pid_t, fd: RawFd) -> Option<OwnedFd> {
    // SAFETY: pidfd_open(2) of a process by its PID, no flags.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let pidfd = RawFd::try_from(pidfd).ok().filter(|&f| f >= 0)?;
    // SAFETY: a descriptor just made, owned here alone.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    // SAFETY: pidfd_getfd(2) of the process's descriptor, no flags.
    let got = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0) };
    let got = RawFd::try_from(got).ok().filter(|&f| f >= 0)?;
    // SAFETY: a descriptor just made, owned here alone.
    Some(unsafe { OwnedFd::from_raw_fd(got) })
}

fn sockopt_raw(fd: RawFd, level: libc::c_int, name: libc::c_int) -> Option<libc::c_int> {
    let mut v: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt(2) into an int of ours, its length given.
    let r = unsafe { libc::getsockopt(fd, level, name, (&raw mut v).cast(), &raw mut len) };
    (r == 0).then_some(v)
}

fn sockopt(sock: &OwnedFd, level: libc::c_int, name: libc::c_int) -> Option<libc::c_int> {
    sockopt_raw(sock.as_raw_fd(), level, name)
}

/// The family and protocol of a TCP or UDP socket of IPv4's or IPv6's; none for any other.
fn inet(sock: &OwnedFd) -> Option<(libc::c_int, u8)> {
    let domain = sockopt(sock, libc::SOL_SOCKET, libc::SO_DOMAIN)?;
    if domain != libc::AF_INET && domain != libc::AF_INET6 {
        return None;
    }
    let proto = match (
        sockopt(sock, libc::SOL_SOCKET, libc::SO_TYPE)?,
        sockopt(sock, libc::SOL_SOCKET, libc::SO_PROTOCOL)?,
    ) {
        (libc::SOCK_STREAM, libc::IPPROTO_TCP) => libc::IPPROTO_TCP as u8,
        (libc::SOCK_DGRAM, libc::IPPROTO_UDP) => libc::IPPROTO_UDP as u8,
        _ => return None,
    };
    Some((domain, proto))
}

/// A u64 socket option of `sock`'s.
fn sockopt64(sock: &OwnedFd, name: libc::c_int) -> Option<u64> {
    let mut v: u64 = 0;
    let mut len = size_of::<u64>() as libc::socklen_t;
    // SAFETY: getsockopt(2) into a u64 of ours, its length given.
    let r = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            (&raw mut v).cast(),
            &raw mut len,
        )
    };
    (r == 0).then_some(v)
}

/// A socket's cookie (SO_COOKIE), as sock_diag names it when it goes.
fn cookie(sock: &OwnedFd) -> Option<u64> {
    sockopt64(sock, libc::SO_COOKIE)
}

/// The cookie of a socket's network namespace (SO_NETNS_COOKIE).
fn netns_cookie(sock: &OwnedFd) -> Option<u64> {
    sockopt64(sock, libc::SO_NETNS_COOKIE)
}

/// The cookie of this process's network namespace, the run's.
fn own_netns() -> Option<u64> {
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    let fd = RawFd::try_from(fd).ok().filter(|&f| f >= 0)?;
    // SAFETY: a descriptor just made, owned here alone.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    netns_cookie(&sock)
}

/// A socket's port, 0 where it is not bound.
fn local_port(sock: &OwnedFd) -> Option<u16> {
    // SAFETY: zeroed is a valid sockaddr_storage.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: getsockname(2) into storage of ours, its length given.
    if unsafe { libc::getsockname(sock.as_raw_fd(), (&raw mut ss).cast(), &raw mut len) } != 0 {
        return None;
    }
    from_storage(&ss).map(|a| a.port())
}

/// Whether a connecting socket has its peer: connected.
fn peer_known(fd: RawFd) -> bool {
    // SAFETY: zeroed is a valid sockaddr_storage.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: getpeername(2) into storage of ours, its length given.
    unsafe { libc::getpeername(fd, (&raw mut ss).cast(), &raw mut len) == 0 }
}

/// A bind's address and port, as the caller passed them (`len` bytes at `at`) for a socket
/// of `domain`; none for one the kernel refuses, which it then does. An IPv4 socket's
/// AF_UNSPEC is its every address where it names that, as inet_bind takes it.
fn read_sockaddr(pid: libc::pid_t, at: u64, len: i32, domain: libc::c_int) -> Option<(IpAddr, u16)> {
    let len = usize::try_from(len).ok()?;
    match domain {
        libc::AF_INET if len >= size_of::<libc::sockaddr_in>() => {
            let mut b = [0u8; size_of::<libc::sockaddr_in>()];
            read_memory(pid, at, &mut b)?;
            // sockaddr_in: family (2), port (2, network order), address (4).
            let [f0, f1, p0, p1, a0, a1, a2, a3, ..] = b;
            let family = i32::from(u16::from_ne_bytes([f0, f1]));
            let addr = Ipv4Addr::new(a0, a1, a2, a3);
            if family != libc::AF_INET && !(family == libc::AF_UNSPEC && addr.is_unspecified()) {
                return None;
            }
            Some((IpAddr::V4(addr), u16::from_be_bytes([p0, p1])))
        }
        libc::AF_INET6 if len >= size_of::<libc::sockaddr_in6>() => {
            let mut b = [0u8; size_of::<libc::sockaddr_in6>()];
            read_memory(pid, at, &mut b)?;
            // sockaddr_in6: family (2), port (2), flow information (4), address (16),
            // scope (4).
            let [f0, f1, p0, p1, _, _, _, _, rest @ ..] = b;
            if i32::from(u16::from_ne_bytes([f0, f1])) != libc::AF_INET6 {
                return None;
            }
            let a = <[u8; 16]>::try_from(rest.get(..16)?).ok()?;
            Some((IpAddr::V6(Ipv6Addr::from(a)), u16::from_be_bytes([p0, p1])))
        }
        _ => None,
    }
}

/// `addr` as a sockaddr, and its length.
fn sockaddr(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: zeroed is a valid sockaddr_storage.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: a sockaddr_in fits in, and is aligned as, sockaddr_storage.
            unsafe { std::ptr::write((&raw mut ss).cast::<libc::sockaddr_in>(), sin) };
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            // SAFETY: zeroed is a valid sockaddr_in6.
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            // SAFETY: a sockaddr_in6 fits in, and is aligned as, sockaddr_storage.
            unsafe { std::ptr::write((&raw mut ss).cast::<libc::sockaddr_in6>(), sin6) };
            size_of::<libc::sockaddr_in6>()
        }
    };
    (ss, len as libc::socklen_t)
}

fn from_storage(ss: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match i32::from(ss.ss_family) {
        libc::AF_INET => {
            // SAFETY: storage of the family AF_INET holds a sockaddr_in.
            let sin = unsafe { &*(&raw const *ss).cast::<libc::sockaddr_in>() };
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes())),
                u16::from_be(sin.sin_port),
            ))
        }
        libc::AF_INET6 => {
            // SAFETY: storage of the family AF_INET6 holds a sockaddr_in6.
            let sin6 = unsafe { &*(&raw const *ss).cast::<libc::sockaddr_in6>() };
            Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)),
                u16::from_be(sin6.sin6_port),
            ))
        }
        _ => None,
    }
}

/// Binds `sock` to `addr` at `port`: its errno where the guest's own kernel refuses it.
fn bind_socket(sock: &OwnedFd, addr: IpAddr, port: u16) -> Result<(), i32> {
    let (ss, len) = sockaddr(SocketAddr::new(addr, port));
    // SAFETY: bind(2) with a sockaddr of ours, its length given.
    if unsafe { libc::bind(sock.as_raw_fd(), (&raw const ss).cast(), len) } == 0 {
        Ok(())
    } else {
        Err(errno())
    }
}

/// Stops a TCP socket listening: connect(2) to AF_UNSPEC, which disconnects it
/// (tcp_disconnect, inet_csk_listen_stop), its port still bound.
fn unlisten(sock: &OwnedFd) {
    // SAFETY: zeroed is a valid sockaddr; its family AF_UNSPEC.
    let unspec: libc::sockaddr = unsafe { std::mem::zeroed() };
    // SAFETY: connect(2) of a socket of ours with a sockaddr of ours.
    unsafe {
        libc::connect(
            sock.as_raw_fd(),
            &raw const unspec,
            size_of::<libc::sockaddr>() as libc::socklen_t,
        )
    };
}

/// A relay's socket on eth0's address `own` at `port`: listening, for TCP; its errno.
fn relay_socket(own: Ipv4Addr, port: u16, udp: bool) -> Result<OwnedFd, i32> {
    let kind = if udp { libc::SOCK_DGRAM } else { libc::SOCK_STREAM };
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_INET, kind | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(errno());
    }
    // SAFETY: a descriptor just made, owned here alone.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    bind_socket(&sock, IpAddr::V4(own), port)?;
    // SAFETY: listen(2) of a socket of ours.
    if !udp && unsafe { libc::listen(sock.as_raw_fd(), libc::SOMAXCONN) } != 0 {
        return Err(errno());
    }
    Ok(sock)
}

/// A socket of `kind` connecting to `target`, from the guest's own loopback where the
/// target is on it; none where it cannot be made.
fn connected(target: SocketAddr, kind: libc::c_int) -> Option<OwnedFd> {
    let family = if target.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(family, kind | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    let fd = RawFd::try_from(fd).ok().filter(|&f| f >= 0)?;
    // SAFETY: a descriptor just made, owned here alone.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let (ss, len) = sockaddr(target);
    // SAFETY: connect(2) of a socket of ours with a sockaddr of ours.
    let r = unsafe { libc::connect(sock.as_raw_fd(), (&raw const ss).cast(), len) };
    (r == 0 || errno() == libc::EINPROGRESS).then_some(sock)
}

/// A datagram from `fd` into `buf`: its length and sender.
fn recv_from(fd: RawFd, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
    // SAFETY: zeroed is a valid sockaddr_storage.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: recvfrom(2) into buffers of ours, of their lengths.
    let n = unsafe {
        libc::recvfrom(
            fd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
            (&raw mut ss).cast(),
            &raw mut len,
        )
    };
    let n = usize::try_from(n).ok()?;
    Some((n, from_storage(&ss)?))
}

fn send_to(fd: RawFd, data: &[u8], to: SocketAddr) {
    let (ss, len) = sockaddr(to);
    // SAFETY: sendto(2) of a buffer of ours, to a sockaddr of ours.
    unsafe {
        libc::sendto(
            fd,
            data.as_ptr().cast(),
            data.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            (&raw const ss).cast(),
            len,
        )
    };
}

/// The cookies of the sockets sock_diag's messages in `msgs` name: each an nlmsghdr (its
/// length, 4 bytes, first), then an inet_diag_msg.
fn cookies(mut msgs: &[u8]) -> Vec<u64> {
    let word = |b: &[u8], at: usize| {
        b.get(at..at + 4)
            .and_then(|w| <[u8; 4]>::try_from(w).ok())
            .map(u32::from_ne_bytes)
    };
    let mut out = Vec::new();
    while let Some(len) = word(msgs, 0).and_then(|l| usize::try_from(l).ok()) {
        if len < 16 {
            break;
        }
        let which = msgs
            .get(4..6)
            .and_then(|k| <[u8; 2]>::try_from(k).ok())
            .map(u16::from_ne_bytes);
        if which == Some(SOCK_DIAG_BY_FAMILY)
            && let (Some(lo), Some(hi)) = (word(msgs, COOKIE_AT), word(msgs, COOKIE_AT + 4))
        {
            out.push(u64::from(lo) | u64::from(hi) << 32);
        }
        msgs = msgs.get(len.next_multiple_of(4)..).unwrap_or_default();
    }
    out
}

/// A sock_diag socket in the destroy groups: each TCP and UDP socket of init's network
/// namespace as it goes.
fn destroy_listener() -> io::Result<OwnedFd> {
    let sock = diag_socket()?;
    // SAFETY: zeroed is a valid sockaddr_nl.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = DESTROY_GROUPS;
    // SAFETY: bind(2) of a sockaddr_nl of ours, its size given.
    if unsafe {
        libc::bind(
            sock.as_raw_fd(),
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

fn diag_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The cookies of every TCP and UDP socket of the guest's network namespace, IPv4's and
/// IPv6's, in every state (sock_diag's dump, inet_diag_req_v2); none where they could
/// not all be listed.
fn sockets() -> Option<HashSet<u64>> {
    let sock = diag_socket().ok()?;
    let mut live = HashSet::new();
    let mut buf = vec![0u8; 32 * 1024];
    for family in [libc::AF_INET, libc::AF_INET6] {
        for proto in [libc::IPPROTO_TCP, libc::IPPROTO_UDP] {
            // nlmsghdr (16), then inet_diag_req_v2: family, protocol, extensions, pad,
            // states (every one), and its socket ID, zeroed (48).
            let mut req = Vec::with_capacity(72);
            req.extend_from_slice(&72u32.to_ne_bytes());
            req.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
            req.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
            req.extend_from_slice(&[0; 8]);
            req.extend_from_slice(&[family as u8, proto as u8, 0, 0]);
            req.extend_from_slice(&u32::MAX.to_ne_bytes());
            req.extend_from_slice(&[0; 48]);
            // SAFETY: send(2) of a buffer of ours, to the kernel.
            if unsafe { libc::send(sock.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) } < 0 {
                return None;
            }
            'dump: loop {
                // SAFETY: recv(2) into a buffer of ours, of its length.
                let n = unsafe { libc::recv(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                let n = usize::try_from(n).ok().filter(|&n| n > 0)?;
                let mut msgs = buf.get(..n).unwrap_or_default();
                while let Some(len) = msgs
                    .get(..4)
                    .and_then(|l| <[u8; 4]>::try_from(l).ok())
                    .map(u32::from_ne_bytes)
                    .and_then(|l| usize::try_from(l).ok())
                {
                    let which = msgs
                        .get(4..6)
                        .and_then(|k| <[u8; 2]>::try_from(k).ok())
                        .map(u16::from_ne_bytes);
                    match which {
                        Some(NLMSG_DONE) => break 'dump,
                        Some(NLMSG_ERROR) => return None,
                        _ => {}
                    }
                    if len < 16 {
                        return None;
                    }
                    live.extend(cookies(msgs.get(..len).unwrap_or_default()));
                    msgs = msgs.get(len.next_multiple_of(4)..).unwrap_or_default();
                }
            }
        }
    }
    Some(live)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Where a bind of the guest's goes on the host (D124): its every address and its own
    /// address the host's every one, reached on eth0; its loopback the host's, relayed;
    /// IPv6's every address alone relayed to its loopback; any other address the guest's.
    #[test]
    fn a_bind_goes_where_its_address_says() {
        let own = Some(Ipv4Addr::new(172, 17, 0, 2));
        let v4 = |a: [u8; 4]| IpAddr::from(a);
        let v6 = IpAddr::V6;
        let any4 = v4([0, 0, 0, 0]);
        let any6 = v6(Ipv6Addr::UNSPECIFIED);
        let lo4 = v4([127, 0, 0, 1]);
        let lo6 = v6(Ipv6Addr::LOCALHOST);
        let own6: Ipv6Addr = "fd00:5ba5::2".parse().unwrap();
        let mapping = |guest, v6only| mapping(guest, v6only, own, Some(own6));
        let m = |host, direct, target, hold, hold6| Mapping {
            host,
            direct,
            target,
            hold,
            hold6,
        };
        let (every, one) = (Some(Ipv4Addr::UNSPECIFIED), Some(Ipv4Addr::LOCALHOST));
        assert_eq!(mapping(any4, false), Some(m(any4, true, any4, every, false)));
        assert_eq!(mapping(lo4, false), Some(m(lo4, false, lo4, one, false)));
        let eth0 = v4([172, 17, 0, 2]);
        assert_eq!(mapping(eth0, false), Some(m(any4, true, eth0, None, false)));
        assert_eq!(mapping(v4([172, 17, 0, 9]), false), None, "another's address");
        assert_eq!(mapping(any6, false), Some(m(any6, true, any6, every, true)));
        assert_eq!(
            mapping(any6, true),
            Some(m(any6, false, lo6, None, true)),
            "IPv6's alone"
        );
        assert_eq!(mapping(lo6, false), Some(m(lo6, false, lo6, None, true)));
        assert_eq!(
            mapping(v6(own6), false),
            Some(m(any6, false, v6(own6), None, false)),
            "eth0's own IPv6 address"
        );
        let mapped = v6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
        assert_eq!(mapping(mapped, false), Some(m(lo4, false, mapped, one, false)));
        assert_eq!(mapping(v6("fe80::1".parse().unwrap()), false), None, "link-local");
    }

    /// A destroy message's cookie, where sock_diag puts it (inet_diag_msg_common_fill,
    /// sock_diag_save_cookie: its low word first); a message of another type skipped.
    #[test]
    fn a_gone_sockets_cookie_is_read_from_its_message() {
        let message = |which: u16, cookie: u64| {
            let mut m = vec![0u8; 16 + 72];
            m.splice(0..4, 88u32.to_ne_bytes());
            m.splice(4..6, which.to_ne_bytes());
            m.splice(COOKIE_AT..COOKIE_AT + 4, (cookie as u32).to_ne_bytes());
            m.splice(
                COOKIE_AT + 4..COOKIE_AT + 8,
                ((cookie >> 32) as u32).to_ne_bytes(),
            );
            m
        };
        let mut three = message(SOCK_DIAG_BY_FAMILY, 0x1234_5678_9abc_def0);
        three.extend(message(NLMSG_DONE, 7));
        three.extend(message(SOCK_DIAG_BY_FAMILY, 42));
        assert_eq!(cookies(&three), vec![0x1234_5678_9abc_def0, 42]);
        assert_eq!(cookies(&[1, 0, 0]), Vec::<u64>::new(), "short");
    }

    /// bind(2) and listen(2) read from a notification on each ABI; socketcall(2)'s from
    /// the caller's memory, this process's own here.
    #[test]
    fn a_calls_arguments_are_read_as_the_kernel_takes_them() {
        let note = |arch: u32, nr: i32, args: [u64; 6]| {
            // SAFETY: zeroed is a valid seccomp_notif.
            let mut n: libc::seccomp_notif = unsafe { std::mem::zeroed() };
            n.pid = std::process::id();
            n.data.arch = arch;
            n.data.nr = nr;
            n.data.args = args;
            n
        };
        let bind = |c: Option<Call>| match c {
            Some(Call::Bind { fd, addr, len }) => Some((fd, addr, len)),
            _ => None,
        };
        let listen = |c: Option<Call>| match c {
            Some(Call::Listen { fd, backlog }) => Some((fd, backlog)),
            _ => None,
        };
        // An int is its register's low 32 bits.
        assert_eq!(
            bind(call(&note(
                AUDIT_ARCH_X86_64,
                49,
                [0x1_0000_0003, 0x7000, 16, 0, 0, 0]
            ))),
            Some((3, 0x7000, 16))
        );
        assert_eq!(
            bind(call(&note(AUDIT_ARCH_X86_64, X32 | 49, [4, 0x7000, 28, 0, 0, 0]))),
            Some((4, 0x7000, 28))
        );
        assert_eq!(
            listen(call(&note(AUDIT_ARCH_AARCH64, 201, [5, 128, 0, 0, 0, 0]))),
            Some((5, 128))
        );
        assert_eq!(
            listen(call(&note(AUDIT_ARCH_ARM, 284, [6, 4096, 0, 0, 0, 0]))),
            Some((6, 4096))
        );
        assert!(call(&note(AUDIT_ARCH_X86_64, 42, [0; 6])).is_none(), "connect");
        // socketcall(SYS_BIND, args) and (SYS_LISTEN, args), the args in memory.
        let words: [u32; 3] = [7, 0x9000, 16];
        let at = words.as_ptr() as u64;
        assert_eq!(
            bind(call(&note(AUDIT_ARCH_I386, 102, [SYS_BIND, at, 0, 0, 0, 0]))),
            Some((7, 0x9000, 16))
        );
        assert_eq!(
            listen(call(&note(AUDIT_ARCH_I386, 102, [SYS_LISTEN, at, 0, 0, 0, 0]))),
            Some((7, 0x9000))
        );
        assert!(
            call(&note(AUDIT_ARCH_I386, 102, [3, at, 0, 0, 0, 0])).is_none(),
            "SYS_CONNECT"
        );
    }
}
