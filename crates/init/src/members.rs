//! The containers a microVM's own engine makes, as members of the microVM's switch
//! (D181), each as each of the image's agents is (D59, D61): a network namespace of its
//! own, linked to the switch through a gate of its own, known by its link and never by an
//! address it could claim, granted what it declares and nothing else.
//!
//! Init makes a member's namespace, its gate and its link before any process of it runs
//! ([`join`]); the joiner's child enters it. Its engine's default network names no peers,
//! so a member on it is alone, as an agent with no `CONNECT` is. It reaches past the
//! microVM as far as the run's command does, and no farther; the command reaches it on
//! the ports it publishes alone, through a proxy of init's in the command's namespace
//! ([`publish`]).

use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::links::{Reach, in_netns, open_ns};

/// Whether the run's own processes are kept from what its agents' grants reach past the
/// microVM (`confine-eth0`, D59): its engine's members reach no farther than they.
pub static CONFINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The name the engine's clients give its default network.
pub const DEFAULT: &str = "bridge";

/// What a member publishes, from the command's network namespace: its protocol's number,
/// the address there (none for every one) and port (0 for one the kernel picks), and its
/// own port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub proto: u8,
    pub host: Option<IpAddr>,
    pub host_port: u16,
    pub port: u16,
}

/// What a joiner's setup asks of its membership (`member=NETWORK\0NAME[\0ALIAS...]`): the
/// network of its engine's it joins, and its names there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub network: String,
    pub names: Vec<String>,
}

/// The membership `setup` asks for, where it asks for one.
pub fn asked(setup: &[Vec<u8>]) -> Result<Option<Asked>, String> {
    let Some(entry) = setup.iter().find_map(|e| e.strip_prefix(b"member=")) else {
        return Ok(None);
    };
    let text = std::str::from_utf8(entry).map_err(|_| "a membership that is not UTF-8".to_string())?;
    let mut parts = text.split('\0');
    let network = parts.next().unwrap_or_default().to_string();
    let names: Vec<String> = parts.map(str::to_string).collect();
    if network.is_empty() || names.iter().any(String::is_empty) {
        return Err(format!("a malformed membership: {text:?}"));
    }
    Ok(Some(Asked { network, names }))
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

/// A member, joined: init's descriptor of the namespace made for it, which its joiner's
/// child enters and init holds while it is a member; its network, and its address, prefix
/// and gateway there; its link's hardware address, the kernel's; and whether it asks the
/// agents' resolver for names past the microVM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    pub ns: RawFd,
    pub network: String,
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub mac: [u8; 6],
    pub dns: bool,
}

/// A network of the engine's: its name, its subnet and its gateway, the subnet's first
/// address.
#[derive(Debug, Clone)]
struct Net {
    name: String,
    subnet: (Ipv4Addr, u8),
    gateway: Ipv4Addr,
}

/// A member, as init keeps it: its joiner's id, its link's index, its network and address
/// there, and its namespace.
#[derive(Debug)]
struct Kept {
    id: u32,
    index: usize,
    network: String,
    addr: Ipv4Addr,
    _ns: OwnedFd,
}

/// The engine's networks and members.
struct Engine {
    networks: Vec<Net>,
    members: Vec<Kept>,
}

static ENGINE: Mutex<Engine> = Mutex::new(Engine {
    networks: Vec::new(),
    members: Vec::new(),
});

fn engine() -> std::sync::MutexGuard<'static, Engine> {
    ENGINE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The first /24 of the switch's block (netplan's, the agents' own) that overlaps none of
/// `taken`.
fn free_subnet(taken: &[(Ipv4Addr, u8)]) -> Option<(Ipv4Addr, u8)> {
    let (base, bits) = crate::netplan::POOL;
    let count = 1u32.checked_shl(u32::from(crate::netplan::POOL_PREFIX - bits))?;
    let step = 1u32.checked_shl(u32::from(32 - crate::netplan::POOL_PREFIX))?;
    (0..count)
        .filter_map(|i| u32::from(base).checked_add(i.checked_mul(step)?))
        .map(|a| (Ipv4Addr::from(a), crate::netplan::POOL_PREFIX))
        .find(|s| !taken.iter().any(|t| crate::netplan::overlaps(*s, *t)))
}

/// The lowest address of `subnet` past its gateway, `.1`, that `held` holds none of; none
/// past its last host address.
fn free_address(subnet: (Ipv4Addr, u8), held: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    let size = 1u32.checked_shl(32 - u32::from(subnet.1))?;
    (2..size.saturating_sub(1))
        .filter_map(|i| u32::from(subnet.0).checked_add(i))
        .map(Ipv4Addr::from)
        .find(|a| !held.contains(a))
}

/// The switch, the domains' where they made one, else one made now with none of theirs.
fn switch() -> io::Result<&'static crate::links::Switch> {
    if let Some(s) = crate::domains::SWITCH.get() {
        return Ok(s);
    }
    let made = crate::links::Switch::new(&[], &[], &[], &[], None, false, 0, &[])?;
    Ok(crate::domains::SWITCH.get_or_init(|| made))
}

/// Makes joiner `id` a member of its engine's network `asked` names, publishing
/// `published` to the run's command: its namespace, its address there, its gate and link,
/// and its grants, each before any process of it runs.
pub fn join(id: u32, asked: &Asked, published: &[crate::netplan::Egress]) -> Result<Joined, String> {
    if asked.network != DEFAULT {
        return Err(format!(
            "network {}: a microVM's engine makes no networks of its own yet, so its containers are on its default network, `{DEFAULT}`",
            asked.network
        ));
    }
    let failed = |what: &str, e: io::Error| format!("{what}: {e}");
    let sw = switch().map_err(|e| failed("the microVM's switch", e))?;
    // Past the microVM by the switch's link up, and reached by the command down it.
    sw.uplink_once()
        .map_err(|e| failed("the switch's link past the microVM", e))?;
    let (eth0, eth0_prefix, _) = crate::net::current()
        .ok_or_else(|| "the microVM has no address for its engine's containers".to_string())?;
    let mask = u32::MAX.checked_shl(32 - u32::from(eth0_prefix)).unwrap_or(0);
    let eth0_subnet = (Ipv4Addr::from(u32::from(eth0) & mask), eth0_prefix);
    let mut e = engine();
    let net = match e.networks.iter().find(|n| n.name == asked.network) {
        Some(n) => n.clone(),
        None => {
            let mut taken = sw.subnets();
            taken.push(eth0_subnet);
            let subnet = free_subnet(&taken).ok_or_else(|| {
                format!(
                    "network {}: no /24 of {}/{} is clear of the microVM's own networks",
                    asked.network,
                    crate::netplan::POOL.0,
                    crate::netplan::POOL.1
                )
            })?;
            let n = Net {
                name: asked.network.clone(),
                subnet,
                gateway: Ipv4Addr::from(u32::from(subnet.0).saturating_add(1)),
            };
            e.networks.push(n.clone());
            n
        }
    };
    let held: Vec<Ipv4Addr> = e
        .members
        .iter()
        .filter(|m| m.network == net.name)
        .map(|m| m.addr)
        .collect();
    let addr = free_address(net.subnet, &held).ok_or_else(|| {
        format!(
            "network {}: every address of {}/{} is held",
            net.name, net.subnet.0, net.subnet.1
        )
    })?;
    let index = (sw.domains()..)
        .find(|i| !e.members.iter().any(|m| m.index == *i))
        .ok_or_else(|| "no link left on the microVM's switch".to_string())?;
    // A namespace of its own, its loopback up, which nothing but this descriptor holds
    // until the joiner's child enters it.
    let ns = in_netns(None, || {
        let ns = open_ns("/proc/thread-self/ns/net")?;
        crate::run::loopback_up()?;
        Ok(ns)
    })
    .map_err(|e| failed("its network namespace", e))?;
    let reach = if CONFINED.load(std::sync::atomic::Ordering::Relaxed) {
        Reach::Within(eth0_subnet)
    } else {
        Reach::AsTheCommand
    };
    let dns = reach != Reach::Nowhere;
    let egress = match reach {
        Reach::Nowhere => Vec::new(),
        _ => crate::links::EVERY_PORT.to_vec(),
    };
    let link = crate::netplan::Link {
        addresses: vec![crate::netplan::Address {
            network: net.name.clone(),
            addr,
            subnet: net.subnet.0,
            prefix: net.subnet.1,
            gateway: net.gateway,
        }],
        addresses6: Vec::new(),
        hosts: Vec::new(),
        egress: egress.clone(),
        ingress: Vec::new(),
        dns,
        mcp: Vec::new(),
        connects: false,
        accepts: false,
        unix: Vec::new(),
    };
    let gate = crate::links::Gate {
        own: vec![addr],
        opens: Vec::new(),
        accepts: Vec::new(),
        egress,
        ingress: published.to_vec(),
        dns,
        own6: Vec::new(),
        opens6: Vec::new(),
        accepts6: Vec::new(),
        egress6: false,
    };
    sw.attach_ns(index, &ns, &link, &gate)
        .map_err(|e| failed("its link to the microVM's switch", e))?;
    let admitted = sw.admit(
        crate::links::Member {
            index,
            addrs: vec![addr],
            reach,
            published: published.to_vec(),
            dns,
        },
        net.subnet,
    );
    let asks = admitted.and_then(|()| {
        if !dns {
            return Ok(());
        }
        let asker = crate::agentdns::Asker {
            link: crate::links::link_index(index)?,
            any: true,
            names: Vec::new(),
            addrs: vec![addr],
        };
        if crate::agentdns::running() {
            return crate::agentdns::admit(&asker);
        }
        let (_, _, upstream) =
            crate::net::current().ok_or_else(|| io::Error::other("the microVM has no resolver"))?;
        let sockets = sw.resolver_sockets(SocketAddr::from((upstream, 53)))?;
        crate::agentdns::start(sockets, vec![asker]).map_err(io::Error::other)
    });
    let mac = asks.and_then(|()| hardware_address(&ns));
    let mac = match mac {
        Ok(mac) => mac,
        Err(why) => {
            let _ = sw.release(index);
            return Err(failed("its grants on the microVM's switch", why));
        }
    };
    let raw = ns.as_raw_fd();
    e.members.push(Kept {
        id,
        index,
        network: net.name.clone(),
        addr,
        _ns: ns,
    });
    Ok(Joined {
        ns: raw,
        network: net.name.clone(),
        addr,
        prefix: net.subnet.1,
        gateway: net.gateway,
        mac,
        dns,
    })
}

/// Lets joiner `id` go, if it is a member: its grants, then its gate and namespace.
pub fn leave(id: u32) {
    let mut e = engine();
    let Some(at) = e.members.iter().position(|m| m.id == id) else {
        return;
    };
    let gone = e.members.remove(at);
    drop(e);
    if let Some(sw) = crate::domains::SWITCH.get() {
        if let Ok(link) = crate::links::link_index(gone.index) {
            let _ = crate::agentdns::release(link);
        }
        let _ = sw.release(gone.index);
    }
}

/// `eth0`'s hardware address in namespace `ns`, as its kernel gave it (SIOCGIFHWADDR).
fn hardware_address(ns: &OwnedFd) -> io::Result<[u8; 6]> {
    in_netns(Some(ns.as_raw_fd()), || {
        // SAFETY: socket(2) with constant arguments.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: an all-zero ifreq is a valid value.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        for (to, from) in req.ifr_name.iter_mut().zip(b"eth0".iter()) {
            *to = *from as libc::c_char;
        }
        // SAFETY: ioctl(2) of an ifreq of ours on our own socket.
        if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFHWADDR as libc::Ioctl, &raw mut req) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel filled the union's hardware address.
        let data = unsafe { req.ifr_ifru.ifru_hwaddr.sa_data };
        let mut mac = [0u8; 6];
        // `c_char` is `u8` on some architectures and `i8` on others: its byte either way.
        for (to, from) in mac.iter_mut().zip(data.iter()) {
            *to = u8::from_ne_bytes(from.to_ne_bytes());
        }
        Ok(mac)
    })
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

/// How long the proxy keeps a UDP client's mapping idle: as long as the microVM's own
/// connection tracking keeps a UDP flow (`nf_conntrack_udp_timeout` until an answer comes,
/// `nf_conntrack_udp_timeout_stream` after), read from the running kernel; where it has no
/// connection tracking, its defaults (net/netfilter/nf_conntrack_proto_udp.c: 30 s, 120 s).
fn udp_idle() -> (Duration, Duration) {
    let read = |key: &str, default: u64| {
        crate::setup::read_sysctl(key)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Duration::from_secs(default), Duration::from_secs)
    };
    (
        read("net.netfilter.nf_conntrack_udp_timeout", 30),
        read("net.netfilter.nf_conntrack_udp_timeout_stream", 120),
    )
}

/// Publishes `bindings` from the namespace of `workload`'s process to `target`, a
/// member's address, in a process of init's own: its PID and the port each binding was
/// given there, in their order; or why one could not be bound.
pub fn publish(
    workload: libc::pid_t,
    target: Ipv4Addr,
    bindings: &[Binding],
) -> Result<(libc::pid_t, Vec<u16>), String> {
    let theirs =
        open_ns(&format!("/proc/{workload}/ns/net")).map_err(|e| format!("the command's network: {e}"))?;
    let idle = udp_idle();
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
            serve(bindings, target, idle, told);
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
/// u16) or why not (`E` and the reason), then each relayed until the process is killed.
fn serve(bindings: &[Binding], target: Ipv4Addr, idle: (Duration, Duration), mut told: std::fs::File) {
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
                let proto = if b.proto == 6 { "tcp" } else { "udp" };
                let why = if e.kind() == io::ErrorKind::AddrInUse {
                    format!(
                        "{at}/{proto} is taken in the command's network: another of its programs, or another container its engine made, listens there"
                    )
                } else {
                    format!("{at}/{proto} in the command's network: {e}")
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
                    Bound::Udp(sock, port) => {
                        relay_udp(sock, SocketAddr::new(IpAddr::V4(target), *port), idle)
                    }
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

/// A UDP client's mapping: its socket to the member, when either side last sent, and
/// whether the member has answered.
struct Mapping {
    out: UdpSocket,
    last: Instant,
    answered: bool,
}

/// Each client's datagrams to `sock` passed on to `to` from a socket of its own, and the
/// answers back to it, until the mapping has been idle as long as `idle` says: its first
/// before an answer, its second after.
fn relay_udp(sock: &UdpSocket, to: SocketAddr, idle: (Duration, Duration)) {
    let clients: Mutex<std::collections::HashMap<SocketAddr, Mapping>> =
        Mutex::new(std::collections::HashMap::new());
    let held = || clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut buf = vec![0u8; 65536];
    std::thread::scope(|s| {
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let datagram = buf.get(..n).unwrap_or_default();
            let mut known = held();
            if let Some(m) = known.get_mut(&from) {
                m.last = Instant::now();
                let _ = m.out.send(datagram);
                continue;
            }
            let Ok(out) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|o| o.connect(to).map(|()| o))
            else {
                continue;
            };
            let _ = out.send(datagram);
            let Ok(back) = out.try_clone() else { continue };
            known.insert(
                from,
                Mapping {
                    out,
                    last: Instant::now(),
                    answered: false,
                },
            );
            drop(known);
            let held = &held;
            let _ = std::thread::Builder::new()
                .name("port".into())
                .spawn_scoped(s, move || {
                    let mut answer = vec![0u8; 65536];
                    loop {
                        // Woken as the mapping could next go idle.
                        let wait = {
                            let known = held();
                            let Some(m) = known.get(&from) else { return };
                            let limit = if m.answered { idle.1 } else { idle.0 };
                            limit.saturating_sub(m.last.elapsed())
                        };
                        if wait.is_zero() {
                            break;
                        }
                        let _ = back.set_read_timeout(Some(wait));
                        match back.recv(&mut answer) {
                            Ok(n) => {
                                if let Some(m) = held().get_mut(&from) {
                                    m.last = Instant::now();
                                    m.answered = true;
                                }
                                let _ = sock.send_to(answer.get(..n).unwrap_or_default(), from);
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => break,
                        }
                    }
                    held().remove(&from);
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
    fn a_members_network_and_ports_are_read_as_the_daemon_writes_them() {
        let setup = vec![
            entry(&["member=bridge", "db", "cache"]),
            entry(&["own-publish=tcp", "", "0", "80"]),
            entry(&["own-publish=udp", "127.0.0.1", "53", "53"]),
        ];
        assert_eq!(
            asked(&setup).unwrap(),
            Some(Asked {
                network: "bridge".into(),
                names: vec!["db".into(), "cache".into()],
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
        assert!(asked(&[entry(&["member=", "db"])]).is_err());
        assert!(asked(&[entry(&["member=bridge", ""])]).is_err());
        assert_eq!(asked(&[]).unwrap(), None);
    }

    /// A network's subnet is the first /24 of the switch's block clear of every one taken,
    /// its members' addresses the lowest free past its gateway.
    #[test]
    fn subnets_and_addresses_are_the_lowest_free() {
        let first = (Ipv4Addr::new(10, 244, 0, 0), 24);
        assert_eq!(free_subnet(&[]), Some(first));
        assert_eq!(
            free_subnet(&[first, (Ipv4Addr::new(10, 244, 1, 0), 24)]),
            Some((Ipv4Addr::new(10, 244, 2, 0), 24))
        );
        // A wider one taken takes every /24 it holds.
        assert_eq!(
            free_subnet(&[(Ipv4Addr::new(10, 244, 0, 0), 23)]),
            Some((Ipv4Addr::new(10, 244, 2, 0), 24))
        );
        assert_eq!(free_subnet(&[(Ipv4Addr::new(10, 0, 0, 0), 8)]), None);
        assert_eq!(free_address(first, &[]), Some(Ipv4Addr::new(10, 244, 0, 2)));
        assert_eq!(
            free_address(
                first,
                &[Ipv4Addr::new(10, 244, 0, 2), Ipv4Addr::new(10, 244, 0, 4)]
            ),
            Some(Ipv4Addr::new(10, 244, 0, 3))
        );
        let tiny = (Ipv4Addr::new(10, 244, 9, 0), 30);
        assert_eq!(free_address(tiny, &[Ipv4Addr::new(10, 244, 9, 2)]), None);
    }
}
