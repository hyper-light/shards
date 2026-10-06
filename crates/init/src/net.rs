//! The guest's network, set up once, before any snapshot, so that a restore does no
//! network work (networking.md R3): `eth0` up with the device's MTU, a static address and
//! the default route through the gateway, by rtnetlink, with no DHCP and no duplicate
//! address detection. The host names the address on the kernel command line,
//! `shards_net=ADDR/PREFIX,GATEWAY`.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLMSG_ERROR: u16 = 2;
const IFLA_MTU: u16 = 4;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_BROADCAST: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_OIF: u16 = 4;
const RT_TABLE_MAIN: u8 = 254;
const RTPROT_BOOT: u8 = 3;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RTN_UNICAST: u8 = 1;
/// The device's MTU (vmm virtio-net, MTU).
const MTU: u32 = 65520;

/// The guest's address, prefix and gateway, from `shards_net=ADDR/PREFIX,GATEWAY`.
pub fn from_cmdline() -> Option<(Ipv4Addr, u8, Ipv4Addr)> {
    parse(&std::env::var("shards_net").ok()?)
}

/// The address a run moved the guest to ([`readdress`]), if any.
static MOVED: std::sync::Mutex<Option<(Ipv4Addr, u8, Ipv4Addr)>> = std::sync::Mutex::new(None);

/// The guest's address, prefix and gateway now: its run's, or its template's.
pub fn current() -> Option<(Ipv4Addr, u8, Ipv4Addr)> {
    let moved = *MOVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    moved.or_else(from_cmdline)
}

/// Brings `eth0` up as the host named it.
pub fn configure(addr: Ipv4Addr, prefix: u8, gateway: Ipv4Addr) -> io::Result<()> {
    // SAFETY: if_nametoindex(3) with a NUL-terminated name.
    let index = unsafe { libc::if_nametoindex(c"eth0".as_ptr()) };
    if index == 0 {
        return Err(io::Error::other("no eth0"));
    }
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let index_i32 = i32::try_from(index).map_err(|_| io::Error::other("an interface index past i32"))?;
    // No IPv6 on eth0, as Docker's bridge has none (its containers' eth0 has disable_ipv6
    // set; measured under Docker Desktop, 2026-10-02): no link-local address, no
    // duplicate address detection to wait on. Before the link is up, so none is made.
    std::fs::write("/proc/sys/net/ipv6/conf/eth0/disable_ipv6", "1")?;

    // The link: up, at the device's MTU (struct ifinfomsg: family, pad, type, index, flags,
    // change).
    let mut link = Vec::new();
    link.extend_from_slice(&[0u8, 0]);
    link.extend_from_slice(&0u16.to_ne_bytes());
    link.extend_from_slice(&index_i32.to_ne_bytes());
    link.extend_from_slice(&(libc::IFF_UP as u32).to_ne_bytes());
    link.extend_from_slice(&(libc::IFF_UP as u32).to_ne_bytes());
    attr(&mut link, IFLA_MTU, &MTU.to_ne_bytes());
    request(&sock, RTM_NEWLINK, 0, &link)?;

    // The address (struct ifaddrmsg: family, prefix length, flags, scope, index).
    let mut a = vec![libc::AF_INET as u8, prefix, 0, 0];
    a.extend_from_slice(&index.to_ne_bytes());
    attr(&mut a, IFA_LOCAL, &addr.octets());
    attr(&mut a, IFA_ADDRESS, &addr.octets());
    let mask = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
    attr(&mut a, IFA_BROADCAST, &(u32::from(addr) | mask).to_be_bytes());
    request(&sock, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, &a)?;

    // The default route (struct rtmsg: family, dst and src length, tos, table, protocol,
    // scope, type, flags).
    let mut r = vec![
        libc::AF_INET as u8,
        0,
        0,
        0,
        RT_TABLE_MAIN,
        RTPROT_BOOT,
        RT_SCOPE_UNIVERSE,
        RTN_UNICAST,
    ];
    r.extend_from_slice(&0u32.to_ne_bytes());
    attr(&mut r, RTA_GATEWAY, &gateway.octets());
    attr(&mut r, RTA_OIF, &index.to_ne_bytes());
    request(&sock, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, &r)
}

/// Moves `eth0` from the address its template booted with, `from`, to `to`, and its default
/// route to `to`'s gateway: a run on a network of its own takes the template of the default
/// bridge's, as every run does, and its own address as it starts. The old address goes
/// with its subnet's route; the new one comes with its own.
pub fn readdress(from: (Ipv4Addr, u8), to: (Ipv4Addr, u8, Ipv4Addr)) -> io::Result<()> {
    // SAFETY: if_nametoindex(3) with a NUL-terminated name.
    let index = unsafe { libc::if_nametoindex(c"eth0".as_ptr()) };
    if index == 0 {
        return Err(io::Error::other("no eth0"));
    }
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let address = |addr: Ipv4Addr, prefix: u8| {
        let mut a = vec![libc::AF_INET as u8, prefix, 0, 0];
        a.extend_from_slice(&index.to_ne_bytes());
        attr(&mut a, IFA_LOCAL, &addr.octets());
        attr(&mut a, IFA_ADDRESS, &addr.octets());
        let mask = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
        attr(&mut a, IFA_BROADCAST, &(u32::from(addr) | mask).to_be_bytes());
        a
    };
    request(&sock, RTM_DELADDR, 0, &address(from.0, from.1))?;
    request(
        &sock,
        RTM_NEWADDR,
        NLM_F_CREATE | NLM_F_EXCL,
        &address(to.0, to.1),
    )?;
    let mut r = vec![
        libc::AF_INET as u8,
        0,
        0,
        0,
        RT_TABLE_MAIN,
        RTPROT_BOOT,
        RT_SCOPE_UNIVERSE,
        RTN_UNICAST,
    ];
    r.extend_from_slice(&0u32.to_ne_bytes());
    attr(&mut r, RTA_GATEWAY, &to.2.octets());
    attr(&mut r, RTA_OIF, &index.to_ne_bytes());
    request(&sock, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_REPLACE, &r)?;
    *MOVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(to);
    Ok(())
}

/// `ADDR/PREFIX,GATEWAY`, as the command line and an `address=` setup entry say it.
pub fn parse(v: &str) -> Option<(Ipv4Addr, u8, Ipv4Addr)> {
    let (cidr, gw) = v.split_once(',')?;
    let (addr, prefix) = cidr.split_once('/')?;
    Some((
        addr.parse().ok()?,
        prefix.parse().ok().filter(|p| *p <= 32)?,
        gw.parse().ok()?,
    ))
}

/// Appends a route attribute: length, type, value, padded to four bytes.
fn attr(buf: &mut Vec<u8>, kind: u16, value: &[u8]) {
    let len = u16::try_from(4 + value.len()).unwrap_or(u16::MAX);
    buf.extend_from_slice(&len.to_ne_bytes());
    buf.extend_from_slice(&kind.to_ne_bytes());
    buf.extend_from_slice(value);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// Sends one request and reads its acknowledgment: the kernel's error, if any.
fn request(sock: &OwnedFd, kind: u16, flags: u16, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(16 + payload.len()).map_err(|_| io::Error::other("a request too long"))?;
    let mut msg = Vec::with_capacity(len as usize);
    msg.extend_from_slice(&len.to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK | flags).to_ne_bytes());
    msg.extend_from_slice(&1u32.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg.extend_from_slice(payload);
    // SAFETY: a buffer of the length given, to our own socket (to the kernel: no address).
    if unsafe { libc::send(sock.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = [0u8; 4096];
    // SAFETY: a buffer of the length given.
    let n = unsafe { libc::recv(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    let reply = buf.get(..n).unwrap_or_default();
    let kind = reply
        .get(4..6)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map(u16::from_ne_bytes);
    if kind != Some(NLMSG_ERROR) {
        return Err(io::Error::other("rtnetlink answered with no acknowledgment"));
    }
    let code = reply
        .get(16..20)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map_or(-libc::EIO, i32::from_ne_bytes);
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(-code))
    }
}
