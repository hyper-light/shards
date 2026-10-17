//! Networks of a microVM's own engine (`--use-api-socket`, D181): each container its engine
//! makes has a network namespace of its own, on a bridge in the command's network
//! namespace, its engine's host's, as dockerd's containers are on `docker0` in theirs.
//!
//! As a container's root is built (join.rs), its child makes the bridge where none is
//! ([`join`]): its gateway's address on it, forwarding on, and what its members send past
//! the microVM translated to the command's own address on eth0, so that the microVM's grants
//! hold for it as for the command's. Then a namespace of its own, and a veth between the
//! two: `eth0` its own, its peer the bridge's. What it publishes, a process of init's passes
//! on from the command's namespace to it ([`publish`]), as dockerd's proxy does from its
//! host's.

use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::time::Duration;

use crate::links::{
    Batch, IFLA_IFNAME, IFLA_INFO_DATA, IFLA_INFO_KIND, IFLA_LINKINFO, IFLA_NET_NS_FD, NLM_F_ACK,
    NLM_F_CREATE, NLM_F_EXCL, NLM_F_REQUEST, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWROUTE, SOURCE, VETH_INFO_PEER,
    addr_msg_prefix, attr, be32, compare, exchange, expr, ifinfomsg, in_netns, interface_index, load, masked,
    nested, netlink_socket, nft, on, open_ns, payload, route_msg,
};

/// A link's hardware address (linux/if_link.h IFLA_ADDRESS), and its bridge (IFLA_MASTER).
const IFLA_ADDRESS: u16 = 1;
const IFLA_MASTER: u16 = 10;

/// Whether the run's own processes are kept from what its agents' grants reach past the
/// microVM (`confine-eth0`, D59): its engine's containers are, as the command's are.
pub static CONFINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A container's address on a network of its microVM's engine: its own, its prefix, its
/// gateway, the bridge's, and its link's hardware address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnNet {
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub mac: [u8; 6],
    pub bridge: String,
}

/// What a container its engine made publishes, from the command's network namespace: its
/// protocol's number, the address there (none for every one) and port (0 for one the
/// kernel picks), and its own port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub proto: u8,
    pub host: Option<IpAddr>,
    pub host_port: u16,
    pub port: u16,
}

/// The network `setup` gives a joiner its engine made (`own-net=ADDR/PREFIX\0GATEWAY\0MAC\0BRIDGE`).
pub fn own_net(setup: &[Vec<u8>]) -> Result<Option<OwnNet>, String> {
    let Some(entry) = setup.iter().find_map(|e| e.strip_prefix(b"own-net=")) else {
        return Ok(None);
    };
    let text = std::str::from_utf8(entry).map_err(|_| "a network entry that is not UTF-8".to_string())?;
    let bad = || format!("a malformed network entry: {text:?}");
    let mut parts = text.split('\0');
    let (Some(cidr), Some(gateway), Some(mac), Some(bridge), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(bad());
    };
    let (addr, prefix) = cidr.split_once('/').ok_or_else(bad)?;
    let mut octets = [0u8; 6];
    let mut hex = mac.split(':');
    for o in &mut octets {
        *o = hex
            .next()
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .ok_or_else(bad)?;
    }
    if hex.next().is_some() || bridge.is_empty() || bridge.len() > 15 {
        return Err(bad());
    }
    Ok(Some(OwnNet {
        addr: addr.parse().map_err(|_| bad())?,
        prefix: prefix.parse().ok().filter(|p| *p <= 30).ok_or_else(bad)?,
        gateway: gateway.parse().map_err(|_| bad())?,
        mac: octets,
        bridge: bridge.to_string(),
    }))
}

/// What `setup` publishes (`own-publish=PROTO\0HOSTIP\0HOSTPORT\0PORT`).
pub fn bindings(setup: &[Vec<u8>]) -> Result<Vec<Binding>, String> {
    setup
        .iter()
        .filter_map(|e| e.strip_prefix(b"own-publish="))
        .map(|e| {
            let text = std::str::from_utf8(e).map_err(|_| "a port entry that is not UTF-8".to_string())?;
            let bad = || format!("a malformed port entry: {text:?}");
            let mut parts = text.split('\0');
            let (Some(proto), Some(host), Some(host_port), Some(port), None) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                return Err(bad());
            };
            Ok(Binding {
                proto: match proto {
                    "tcp" => 6,
                    "udp" => 17,
                    _ => return Err(bad()),
                },
                host: if host.is_empty() {
                    None
                } else {
                    Some(host.parse().map_err(|_| bad())?)
                },
                host_port: host_port.parse().map_err(|_| bad())?,
                port: port.parse().map_err(|_| bad())?,
            })
        })
        .collect()
}

/// In a joiner's child, single-threaded, its network: the bridge in the namespace of
/// `workload`'s process where none is, a namespace of its own, and the link between them.
pub fn join(workload: libc::pid_t, net: &OwnNet) -> Result<(), String> {
    let theirs =
        open_ns(&format!("/proc/{workload}/ns/net")).map_err(|e| format!("the command's network: {e}"))?;
    in_netns(Some(theirs.as_raw_fd()), || bridge(net))
        .map_err(|e| format!("its network's bridge {}: {e}", net.bridge))?;
    // SAFETY: unshare(2) of this single-threaded child's network namespace.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(format!(
            "a network namespace of its own: {}",
            io::Error::last_os_error()
        ));
    }
    crate::run::loopback_up().map_err(|e| format!("its loopback: {e}"))?;
    let route = netlink_socket(libc::NETLINK_ROUTE).map_err(|e| format!("its links: {e}"))?;
    // Its peer is named after its address, which no other container on the bridge has.
    let peer = format!("ve{:08x}\0", u32::from(net.addr));
    let create = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    let mut m = ifinfomsg(0, 0, 0);
    attr(&mut m, IFLA_IFNAME, b"eth0\0");
    attr(&mut m, IFLA_ADDRESS, &net.mac);
    nested(&mut m, IFLA_LINKINFO, |li| {
        attr(li, IFLA_INFO_KIND, b"veth");
        nested(li, IFLA_INFO_DATA, |data| {
            nested(data, VETH_INFO_PEER, |p| {
                p.extend_from_slice(&ifinfomsg(0, 0, 0));
                attr(p, IFLA_IFNAME, peer.as_bytes());
                attr(p, IFLA_NET_NS_FD, &(theirs.as_raw_fd() as u32).to_ne_bytes());
            });
        });
    });
    exchange(&route, &[(RTM_NEWLINK, create, m)]).map_err(|e| format!("its link: {e}"))?;
    let eth0 = interface_index("eth0")
        .and_then(|i| i32::try_from(i).map_err(io::Error::other))
        .map_err(|e| format!("its link: {e}"))?;
    let up = libc::IFF_UP as u32;
    exchange(
        &route,
        &[
            (RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, ifinfomsg(eth0, up, up)),
            (RTM_NEWADDR, create, addr_msg_prefix(eth0, net.addr, net.prefix)),
            (
                RTM_NEWROUTE,
                create,
                route_msg(eth0, Ipv4Addr::UNSPECIFIED, 0, Some(net.gateway), Some(net.addr)),
            ),
        ],
    )
    .map_err(|e| format!("its address {}/{}: {e}", net.addr, net.prefix))?;
    // Its peer on the bridge, up.
    let peer_name = peer.trim_end_matches('\0').to_string();
    in_netns(Some(theirs.as_raw_fd()), || {
        let route = netlink_socket(libc::NETLINK_ROUTE)?;
        let at = i32::try_from(interface_index(&peer_name)?).map_err(io::Error::other)?;
        let master = interface_index(&net.bridge)?;
        let mut m = ifinfomsg(at, up, up);
        attr(&mut m, IFLA_MASTER, &master.to_ne_bytes());
        exchange(&route, &[(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, m)])
    })
    .map_err(|e| format!("its link on bridge {}: {e}", net.bridge))
}

/// In a joiner's child, single-threaded: a network namespace of its own with its loopback
/// alone (`--network none`).
pub fn alone() -> Result<(), String> {
    // SAFETY: unshare(2) of this single-threaded child's network namespace.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(format!(
            "a network namespace of its own: {}",
            io::Error::last_os_error()
        ));
    }
    crate::run::loopback_up().map_err(|e| format!("its loopback: {e}"))
}

/// In the command's network namespace: `net`'s bridge, where none is, with its gateway's
/// address, forwarding on, and its members' flows past the microVM leaving as the
/// command's own (source NAT to eth0's address); kept to eth0's own subnet where the
/// command is ([`CONFINED`]). One the bridge was there for already, another container
/// made.
fn bridge(net: &OwnNet) -> io::Result<()> {
    let route = netlink_socket(libc::NETLINK_ROUTE)?;
    let name = format!("{}\0", net.bridge);
    let mut m = ifinfomsg(0, 0, 0);
    attr(&mut m, IFLA_IFNAME, name.as_bytes());
    nested(&mut m, IFLA_LINKINFO, |li| attr(li, IFLA_INFO_KIND, b"bridge"));
    let create = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    match exchange(&route, &[(RTM_NEWLINK, create, m)]) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => return Ok(()),
        other => other?,
    }
    let index = i32::try_from(interface_index(&net.bridge)?).map_err(io::Error::other)?;
    let up = libc::IFF_UP as u32;
    exchange(
        &route,
        &[
            (
                RTM_NEWADDR,
                create,
                addr_msg_prefix(index, net.gateway, net.prefix),
            ),
            (RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, ifinfomsg(index, up, up)),
        ],
    )?;
    crate::setup::write_sysctl("net.ipv4.ip_forward", "1").map_err(io::Error::other)?;
    // Past the microVM by eth0, where the command has it: as the command's own.
    let Ok(eth0) = interface_index("eth0") else {
        return Ok(());
    };
    let Some((own, mask)) = ipv4_of("eth0")? else {
        return Ok(());
    };
    let subnet = u32::from(net.addr) & prefix_mask(net.prefix);
    let nft_sock = netlink_socket(libc::NETLINK_NETFILTER)?;
    let mut b = Batch::new();
    let chain = format!("engine-{}\0", net.bridge);
    b.chain(
        chain.as_bytes(),
        b"nat\0",
        nft::NF_INET_POST_ROUTING,
        nft::PRIORITY_SNAT,
        nft::NF_ACCEPT,
    );
    let mut e = Vec::new();
    on(&mut e, nft::NFT_META_OIF, eth0);
    payload(&mut e, nft::NFT_PAYLOAD_NETWORK_HEADER, SOURCE, 4);
    masked(&mut e, &prefix_mask(net.prefix).to_be_bytes());
    compare(&mut e, nft::NFT_CMP_EQ, &subnet.to_be_bytes());
    load(&mut e, &own.octets());
    expr(&mut e, b"nat\0", |d| {
        be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_SNAT);
        be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV4));
        be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
    });
    b.rule(chain.as_bytes(), e);
    // Beside agents whose grants reach past the microVM, the command reaches no further
    // than eth0's own subnet (D59, `confine_eth0`), and nor do its engine's containers.
    if CONFINED.load(std::sync::atomic::Ordering::Relaxed) {
        let forward = format!("engine-{}-fwd\0", net.bridge);
        b.chain(
            forward.as_bytes(),
            b"filter\0",
            nft::NF_INET_FORWARD,
            0,
            nft::NF_ACCEPT,
        );
        let mut e = Vec::new();
        on(
            &mut e,
            nft::NFT_META_IIF,
            u32::try_from(index).map_err(io::Error::other)?,
        );
        on(&mut e, nft::NFT_META_OIF, eth0);
        payload(
            &mut e,
            nft::NFT_PAYLOAD_NETWORK_HEADER,
            crate::links::DESTINATION,
            4,
        );
        masked(&mut e, &u32::from(mask).to_be_bytes());
        compare(
            &mut e,
            nft::NFT_CMP_NEQ,
            &(u32::from(own) & u32::from(mask)).to_be_bytes(),
        );
        crate::links::drop_verdict(&mut e);
        b.rule(forward.as_bytes(), e);
    }
    b.commit(&nft_sock)
}

/// A prefix's mask.
fn prefix_mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

/// Link `name`'s IPv4 address and mask, in this thread's network namespace, where it has
/// one (SIOCGIFADDR, SIOCGIFNETMASK).
fn ipv4_of(name: &str) -> io::Result<Option<(Ipv4Addr, Ipv4Addr)>> {
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let ask = |request: libc::Ioctl| -> io::Result<Option<Ipv4Addr>> {
        // SAFETY: an all-zero ifreq is a valid value.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        for (to, from) in req.ifr_name.iter_mut().zip(name.bytes().take(libc::IFNAMSIZ - 1)) {
            *to = from as libc::c_char;
        }
        // SAFETY: ioctl(2) of an ifreq of ours on our own socket.
        if unsafe { libc::ioctl(sock.as_raw_fd(), request, &raw mut req) } != 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EADDRNOTAVAIL) {
                Ok(None)
            } else {
                Err(e)
            };
        }
        // SAFETY: the kernel filled the union's address as a sockaddr_in.
        let sin = unsafe { *(&raw const req.ifr_ifru.ifru_addr).cast::<libc::sockaddr_in>() };
        Ok(Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))))
    };
    match (
        ask(libc::SIOCGIFADDR as libc::Ioctl)?,
        ask(libc::SIOCGIFNETMASK as libc::Ioctl)?,
    ) {
        (Some(addr), Some(mask)) => Ok(Some((addr, mask))),
        _ => Ok(None),
    }
}

/// How long a UDP client's mapping is kept with nothing sent either way, as docker-proxy
/// keeps one (moby cmd/docker-proxy udp_proxy.go, UDPConnTrackTimeout, 90 s).
const UDP_IDLE: Duration = Duration::from_secs(90);

/// Publishes `bindings` from the namespace of `workload`'s process to `target`, a
/// container's address, in a process of init's own: its PID and the port each binding was
/// given there, in their order; or why one could not be bound, in dockerd's words.
pub fn publish(
    workload: libc::pid_t,
    target: Ipv4Addr,
    bindings: &[Binding],
) -> Result<(libc::pid_t, Vec<u16>), String> {
    let theirs =
        open_ns(&format!("/proc/{workload}/ns/net")).map_err(|e| format!("the command's network: {e}"))?;
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2(2) fills two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("a pipe: {}", io::Error::last_os_error()));
    }
    let [r, w] = fds;
    // SAFETY: fresh descriptors nothing else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) };
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(format!(
            "forking the ports' proxy: {}",
            io::Error::last_os_error()
        )),
        0 => {
            crate::dnsrelay::keep_only(&[theirs.as_raw_fd(), write.as_raw_fd()]);
            drop(read);
            // SAFETY: setns(2) of this single-threaded child into the command's namespace.
            let entered = unsafe { libc::setns(theirs.as_raw_fd(), libc::CLONE_NEWNET) } == 0;
            drop(theirs);
            let mut told = std::fs::File::from(write);
            if !entered {
                let _ = told.write_all(format!("E{}", io::Error::last_os_error()).as_bytes());
                // SAFETY: _exit(2) of the child, which owns nothing to flush.
                unsafe { libc::_exit(1) }
            }
            serve(bindings, target, told);
            // SAFETY: as above.
            unsafe { libc::_exit(0) }
        }
        pid => {
            drop(write);
            let mut said = Vec::new();
            let _ = std::fs::File::from(read).read_to_end(&mut said);
            match said.split_first().map(|(k, rest)| (k, rest.as_chunks::<2>())) {
                Some((b'P', (ports, []))) if ports.len() == bindings.len() => {
                    Ok((pid, ports.iter().map(|p| u16::from_be_bytes(*p)).collect()))
                }
                _ => {
                    // SAFETY: kill(2) and waitpid(2) of init's own child.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                        libc::waitpid(pid, std::ptr::null_mut(), 0);
                    }
                    Err(String::from_utf8_lossy(said.get(1..).unwrap_or_default()).into_owned())
                }
            }
        }
    }
}

/// The proxy's side: each binding bound, the ports said on `told` (`P` and each a big-endian
/// u16) or why not (`E` and dockerd's words), then each relayed until the process is killed.
fn serve(bindings: &[Binding], target: Ipv4Addr, mut told: std::fs::File) {
    enum Bound {
        Tcp(TcpListener, u16),
        Udp(UdpSocket, u16),
    }
    let mut bound = Vec::with_capacity(bindings.len());
    let mut ports = vec![b'P'];
    for b in bindings {
        let at = SocketAddr::new(b.host.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), b.host_port);
        let made = match b.proto {
            6 => TcpListener::bind(at).and_then(|l| Ok((l.local_addr()?.port(), Bound::Tcp(l, b.port)))),
            _ => UdpSocket::bind(at).and_then(|s| Ok((s.local_addr()?.port(), Bound::Udp(s, b.port)))),
        };
        match made {
            Ok((port, it)) => {
                ports.extend_from_slice(&port.to_be_bytes());
                bound.push(it);
            }
            Err(e) => {
                let why = if e.kind() == io::ErrorKind::AddrInUse {
                    format!("Bind for {at} failed: port is already allocated")
                } else {
                    format!("Bind for {at} failed: {e}")
                };
                let _ = told.write_all(format!("E{why}").as_bytes());
                return;
            }
        }
    }
    if told.write_all(&ports).is_err() {
        return;
    }
    drop(told);
    std::thread::scope(|s| {
        for it in &bound {
            let _ = std::thread::Builder::new()
                .name("ports".into())
                .spawn_scoped(s, move || match it {
                    Bound::Tcp(listener, port) => {
                        relay_tcp(listener, SocketAddr::new(IpAddr::V4(target), *port))
                    }
                    Bound::Udp(sock, port) => relay_udp(sock, SocketAddr::new(IpAddr::V4(target), *port)),
                });
        }
    });
}

/// Each connection to `listener` passed on to `to`, both ways, until either side ends.
fn relay_tcp(listener: &TcpListener, to: SocketAddr) {
    for conn in listener.incoming() {
        let Ok(client) = conn else { continue };
        let _ = std::thread::Builder::new().name("port".into()).spawn(move || {
            let Ok(server) = TcpStream::connect(to) else {
                return;
            };
            let (Ok(mut c_in), Ok(mut s_out)) = (client.try_clone(), server.try_clone()) else {
                return;
            };
            let up = std::thread::Builder::new().name("port up".into()).spawn(move || {
                let _ = io::copy(&mut c_in, &mut s_out);
                let _ = s_out.shutdown(std::net::Shutdown::Write);
            });
            let (mut s_in, mut c_out) = (server, client);
            if up.is_ok() {
                let _ = io::copy(&mut s_in, &mut c_out);
            }
            let _ = c_out.shutdown(std::net::Shutdown::Both);
            if let Ok(up) = up {
                let _ = up.join();
            }
        });
    }
}

/// Each client's datagrams to `sock` passed on to `to` from a socket of its own, and the
/// answers back to it, until it is idle for [`UDP_IDLE`].
fn relay_udp(sock: &UdpSocket, to: SocketAddr) {
    let clients: std::sync::Mutex<std::collections::HashMap<SocketAddr, UdpSocket>> =
        std::sync::Mutex::new(std::collections::HashMap::new());
    let mut buf = vec![0u8; 65536];
    std::thread::scope(|s| {
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let datagram = buf.get(..n).unwrap_or_default();
            let mut known = clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(out) = known.get(&from) {
                let _ = out.send(datagram);
                continue;
            }
            let Ok(out) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|o| o.connect(to).map(|()| o))
            else {
                continue;
            };
            let _ = out.send(datagram);
            let Ok(back) = out.try_clone() else { continue };
            known.insert(from, out);
            drop(known);
            let clients = &clients;
            let _ = std::thread::Builder::new()
                .name("port".into())
                .spawn_scoped(s, move || {
                    let _ = back.set_read_timeout(Some(UDP_IDLE));
                    let mut answer = vec![0u8; 65536];
                    while let Ok(n) = back.recv(&mut answer) {
                        let _ = sock.send_to(answer.get(..n).unwrap_or_default(), from);
                    }
                    clients
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&from);
                });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(parts: &[&str]) -> Vec<u8> {
        parts.join("\0").into_bytes()
    }

    #[test]
    fn a_containers_network_and_ports_are_read_as_the_daemon_writes_them() {
        let setup = vec![
            entry(&[
                "own-net=10.89.0.5/24",
                "10.89.0.1",
                "02:42:0a:59:00:05",
                "shards0",
            ]),
            entry(&["own-publish=tcp", "", "0", "80"]),
            entry(&["own-publish=udp", "127.0.0.1", "53", "53"]),
        ];
        assert_eq!(
            own_net(&setup).unwrap(),
            Some(OwnNet {
                addr: Ipv4Addr::new(10, 89, 0, 5),
                prefix: 24,
                gateway: Ipv4Addr::new(10, 89, 0, 1),
                mac: [2, 0x42, 0x0a, 0x59, 0, 5],
                bridge: "shards0".into(),
            })
        );
        assert_eq!(
            bindings(&setup).unwrap(),
            [
                Binding {
                    proto: 6,
                    host: None,
                    host_port: 0,
                    port: 80
                },
                Binding {
                    proto: 17,
                    host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                    host_port: 53,
                    port: 53
                },
            ]
        );
        assert!(own_net(&[entry(&["own-net=10.89.0.5/24", "x", "zz", "b"])]).is_err());
        assert_eq!(own_net(&[]).unwrap(), None);
    }
}
