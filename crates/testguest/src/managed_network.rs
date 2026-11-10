//! Native socket observations for the managed-runtime route experiments. This does
//! not install product policy. The explicit intercept-tcp4 experiment administers
//! only fresh guest namespaces and records its finite route observations.

use std::io::{self, Read, Write};
use std::mem::{size_of, size_of_val};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

mod intercept;

const CONFIG: &[&str] = &[
    "CONFIG_NETFILTER",
    "CONFIG_NF_TABLES",
    "CONFIG_NF_TABLES_IPV4",
    "CONFIG_NF_TABLES_IPV6",
    "CONFIG_NFT_SOCKET",
    "CONFIG_NFT_TPROXY",
    "CONFIG_NF_SOCKET_IPV4",
    "CONFIG_NF_SOCKET_IPV6",
    "CONFIG_NF_TPROXY_IPV4",
    "CONFIG_NF_TPROXY_IPV6",
    "CONFIG_IP_MULTIPLE_TABLES",
    "CONFIG_IPV6_MULTIPLE_TABLES",
    "CONFIG_CGROUP_BPF",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocketMode {
    Native,
    Transparent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Tcp,
    Udp,
    UdpConnected,
}

#[derive(Debug)]
enum Probe<'a> {
    Capabilities(Option<&'a str>),
    Intercept(intercept::Inputs),
    InterceptUdp(intercept::udp::Inputs),
    SockAddr(intercept::sock_addr::Inputs),
    Serve(Case, SocketMode),
    Dial(Case),
    Pair(Case),
}

#[derive(Debug)]
struct Case {
    transport: Transport,
    address: SocketAddr,
    sent: Vec<u8>,
    received: Vec<u8>,
    within: Duration,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Listening,
    Connected,
    Sent,
    Received,
    WriteClosed,
    PeerClosed,
    Matched,
}

pub fn main() -> i32 {
    let result = arguments().and_then(|args| parse(&args).and_then(run));
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "managed-network failed kind={:?} errno={:?}: {e}",
                e.kind(),
                e.raw_os_error()
            );
            1
        }
    }
}

fn arguments() -> io::Result<Vec<String>> {
    let mut args = Vec::new();
    for arg in std::env::args_os().skip(2) {
        args.try_reserve(1).map_err(io::Error::other)?;
        args.push(
            arg.into_string()
                .map_err(|_| invalid("non-UTF8 probe argument"))?,
        );
    }
    Ok(args)
}

fn invalid(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

fn parse(args: &[String]) -> io::Result<Probe<'_>> {
    let mut words = Vec::new();
    words.try_reserve_exact(args.len()).map_err(io::Error::other)?;
    words.extend(args.iter().map(String::as_str));
    match words.as_slice() {
        [
            "sock-addr-udp4"
            | "sock-addr-client4"
            | "sock-addr-relay4"
            | "sock-addr-cancel4"
            | "sock-addr-precommit4"
            | "sock-addr-boundary4",
            ..,
        ] => Ok(Probe::SockAddr(intercept::sock_addr::Inputs::parse(&words)?)),
        ["capabilities"] => Ok(Probe::Capabilities(None)),
        ["capabilities", path] => Ok(Probe::Capabilities(Some(path))),
        [
            protocol @ ("intercept-tcp4" | "intercept-tcp6"),
            origin,
            millis,
            mark,
            table,
            priority,
        ] => {
            let family = match *protocol {
                "intercept-tcp4" => intercept::Family::Ipv4,
                "intercept-tcp6" => intercept::Family::Ipv6,
                _ => return Err(invalid("unsupported interception family")),
            };
            Ok(Probe::Intercept(intercept::Inputs::parse(
                family, origin, millis, mark, table, priority,
            )?))
        }
        [
            protocol @ ("intercept-udp4" | "intercept-udp6"),
            origin,
            mode,
            millis,
            mark,
            table,
            priority,
        ] => {
            let family = if *protocol == "intercept-udp4" {
                intercept::Family::Ipv4
            } else {
                intercept::Family::Ipv6
            };
            Ok(Probe::InterceptUdp(intercept::udp::Inputs::parse(
                family, origin, mode, millis, mark, table, priority,
            )?))
        }
        [role, transport, address, sent, received, millis, mode] => {
            let transport = match *transport {
                "tcp" => Transport::Tcp,
                "udp" => Transport::Udp,
                "udp-connected" => Transport::UdpConnected,
                _ => return Err(invalid("transport must be tcp, udp or udp-connected")),
            };
            let case = Case {
                transport,
                address: address
                    .parse()
                    .map_err(|_| invalid("expected numeric socket address"))?,
                sent: unhex(sent)?,
                received: unhex(received)?,
                within: Duration::from_millis(
                    millis.parse().map_err(|_| invalid("deadline is milliseconds"))?,
                ),
            };
            match (*role, *mode, transport) {
                ("serve", "native", Transport::Tcp | Transport::Udp) => {
                    Ok(Probe::Serve(case, SocketMode::Native))
                }
                ("serve", "transparent", Transport::Tcp | Transport::Udp) => {
                    Ok(Probe::Serve(case, SocketMode::Transparent))
                }
                ("dial", "native", _) => Ok(Probe::Dial(case)),
                ("pair", "native", _) => Ok(Probe::Pair(case)),
                _ => Err(invalid("unsupported role/socket mode")),
            }
        }
        _ => Err(invalid(
            "use capabilities [CONFIG], sock-addr-udp4 {connected|unconnected} DEADLINE_MS {destination-only|fixed-original-peer}, sock-addr-relay4 {connected|unconnected} DEADLINE_MS {destination-only|fixed-original-peer} BUFFER_BYTES, {intercept-tcp4|intercept-tcp6} {local|forward|local-loopback|local-self|forward-relay-absent} DEADLINE_MS MARK TABLE RULE_PRIORITY, {intercept-udp4|intercept-udp6} {local|forward|local-loopback|local-self} {connected|unconnected} DEADLINE_MS MARK TABLE RULE_PRIORITY, or {serve|dial|pair} {tcp|udp|udp-connected} ADDRESS SENT_HEX EXPECT_HEX DEADLINE_MS {native|transparent}",
        )),
    }
}

fn unhex(text: &str) -> io::Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(invalid("hex must contain whole bytes"));
    }
    let digit = |b| match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(invalid("invalid hex digit")),
    };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(text.len() / 2)
        .map_err(io::Error::other)?;
    for &[high, low] in text.as_bytes().as_chunks::<2>().0 {
        let high = digit(high)?;
        let low = digit(low)?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    cancel: Option<RawFd>,
}

impl Deadline {
    fn new(within: Duration) -> io::Result<Self> {
        if within.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "probe deadline ended"));
        }
        Instant::now()
            .checked_add(within)
            .map(|at| Self { at, cancel: None })
            .ok_or_else(|| invalid("deadline overflow"))
    }

    fn left(&self) -> io::Result<Duration> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::Error::new(io::ErrorKind::TimedOut, "probe deadline ended"))
        } else {
            Ok(left)
        }
    }

    fn io<T>(&self, fd: RawFd, events: i16, mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        loop {
            self.left()?;
            match operation() {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                answer => return answer,
            }
            let left = self.left()?;
            let millis = left.as_nanos().div_ceil(Duration::from_millis(1).as_nanos());
            let timeout = i32::try_from(millis).unwrap_or(i32::MAX);
            let mut poll = [
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.cancel.unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll reads/fills this array; negative descriptors are ignored.
            let result = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, timeout) };
            if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
            if poll.last().is_some_and(|p| p.revents != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "paired peer failed",
                ));
            }
        }
    }
}

fn run(probe: Probe<'_>) -> io::Result<()> {
    match probe {
        Probe::Capabilities(path) => capabilities(path),
        Probe::Intercept(input) => intercept::run(input),
        Probe::InterceptUdp(input) => intercept::udp::run(input),
        Probe::SockAddr(input) => intercept::sock_addr::run(input),
        Probe::Serve(case, mode) => {
            let deadline = Deadline::new(case.within)?;
            match case.transport {
                Transport::Tcp => tcp_serve(case, mode, &deadline),
                Transport::Udp => udp_serve(case, mode, &deadline),
                Transport::UdpConnected => Err(invalid("connected UDP is a client mode")),
            }
        }
        Probe::Dial(case) => {
            let deadline = Deadline::new(case.within)?;
            match case.transport {
                Transport::Tcp => tcp_dial(case, &deadline),
                Transport::Udp | Transport::UdpConnected => udp_dial(case, &deadline),
            }
        }
        Probe::Pair(case) => {
            let deadline = Deadline::new(case.within)?;
            paired(case, deadline)
        }
    }
}

fn event(state: State, details: std::fmt::Arguments<'_>) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "managed-network {state:?} {details}")?;
    out.flush()
}

fn observed(
    bytes: &[u8],
    local: SocketAddr,
    peer: SocketAddr,
    original: Option<SocketAddr>,
) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(
        out,
        "managed-network {:?} local={local} peer={peer} original={original:?} bytes={} hex=",
        State::Received,
        bytes.len()
    )?;
    for byte in bytes {
        write!(out, "{byte:02x}")?;
    }
    writeln!(out)?;
    out.flush()
}

fn tcp_serve(case: Case, mode: SocketMode, deadline: &Deadline) -> io::Result<()> {
    let socket = bound(case.address, libc::SOCK_STREAM, mode)?;
    // SAFETY: socket is an owned TCP socket; this probe admits one connection.
    if unsafe { libc::listen(socket.as_raw_fd(), 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let listener = TcpListener::from(socket);
    tcp_serve_on(case, mode, deadline, listener)
}

fn tcp_serve_on(case: Case, mode: SocketMode, deadline: &Deadline, listener: TcpListener) -> io::Result<()> {
    event(
        State::Listening,
        format_args!("transport=tcp local={} mode={mode:?}", listener.local_addr()?),
    )?;
    let (mut stream, peer) = deadline.io(listener.as_raw_fd(), libc::POLLIN, || listener.accept())?;
    stream.set_nonblocking(true)?;
    let local = stream.local_addr()?;
    event(State::Connected, format_args!("local={local} peer={peer}"))?;
    let got = tcp_receive(&mut stream, &case.received, deadline)?;
    observed(&got, local, peer, Some(local))?;
    tcp_send(&mut stream, &case.sent, deadline)?;
    stream.shutdown(Shutdown::Write)?;
    event(State::WriteClosed, format_args!("transport=tcp"))?;
    event(State::Matched, format_args!("transport=tcp"))
}

fn tcp_dial(case: Case, deadline: &Deadline) -> io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&case.address, deadline.left()?)?;
    stream.set_nonblocking(true)?;
    let (local, peer) = (stream.local_addr()?, stream.peer_addr()?);
    event(State::Connected, format_args!("local={local} peer={peer}"))?;
    tcp_send(&mut stream, &case.sent, deadline)?;
    stream.shutdown(Shutdown::Write)?;
    event(State::WriteClosed, format_args!("transport=tcp"))?;
    let got = tcp_receive(&mut stream, &case.received, deadline)?;
    observed(&got, local, peer, Some(local))?;
    event(State::Matched, format_args!("transport=tcp"))
}

fn tcp_send(stream: &mut TcpStream, mut bytes: &[u8], deadline: &Deadline) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let count = bytes.len();
    while !bytes.is_empty() {
        let n = deadline.io(fd, libc::POLLOUT, || stream.write(bytes))?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "TCP write ended"));
        }
        bytes = bytes.get(n..).ok_or_else(|| invalid("write exceeded input"))?;
    }
    event(State::Sent, format_args!("transport=tcp bytes={count}"))
}

fn buffer(len: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(io::Error::other)?;
    out.resize(len, 0);
    Ok(out)
}

fn tcp_receive(stream: &mut TcpStream, expected: &[u8], deadline: &Deadline) -> io::Result<Vec<u8>> {
    let mut got = buffer(expected.len())?;
    let fd = stream.as_raw_fd();
    let mut left = got.as_mut_slice();
    while !left.is_empty() {
        let n = deadline.io(fd, libc::POLLIN, || stream.read(left))?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TCP payload ended early",
            ));
        }
        left = left.get_mut(n..).ok_or_else(|| invalid("read exceeded buffer"))?;
    }
    if got != expected {
        return Err(invalid("TCP payload differs"));
    }
    let mut extra = [0];
    if deadline.io(fd, libc::POLLIN, || stream.read(&mut extra))? != 0 {
        return Err(invalid("TCP payload has extra bytes"));
    }
    event(State::PeerClosed, format_args!("transport=tcp"))?;
    Ok(got)
}

fn bound(at: SocketAddr, kind: i32, mode: SocketMode) -> io::Result<OwnedFd> {
    let family = if at.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // SAFETY: socket returns a new descriptor, immediately owned below.
    let fd = unsafe { libc::socket(family, kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is the new uniquely owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if mode == SocketMode::Transparent {
        let (level, option) = transparent_option(at.is_ipv4());
        set_option(fd.as_raw_fd(), level, option)?;
    }
    let result = match at {
        SocketAddr::V4(at) => {
            let address = libc::sockaddr_in {
                sin_family: libc::AF_INET as _,
                sin_port: at.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(at.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: bind reads an IPv4 sockaddr of its exact size during this call.
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&raw const address).cast(),
                    size_of::<libc::sockaddr_in>() as _,
                )
            }
        }
        SocketAddr::V6(at) => {
            let address = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as _,
                sin6_port: at.port().to_be(),
                sin6_flowinfo: at.flowinfo().to_be(),
                sin6_addr: libc::in6_addr {
                    s6_addr: at.ip().octets(),
                },
                sin6_scope_id: at.scope_id(),
            };
            // SAFETY: bind reads an IPv6 sockaddr of its exact size during this call.
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&raw const address).cast(),
                    size_of::<libc::sockaddr_in6>() as _,
                )
            }
        }
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

fn transparent_option(v4: bool) -> (i32, i32) {
    if v4 {
        (libc::SOL_IP, libc::IP_TRANSPARENT)
    } else {
        (libc::SOL_IPV6, libc::IPV6_TRANSPARENT)
    }
}

fn set_option(fd: RawFd, level: i32, option: i32) -> io::Result<()> {
    let value: libc::c_int = 1;
    // SAFETY: setsockopt reads the integer of its exact length.
    if unsafe {
        libc::setsockopt(
            fd,
            level,
            option,
            (&raw const value).cast(),
            size_of::<libc::c_int>() as _,
        )
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn udp_socket(at: SocketAddr, mode: SocketMode) -> io::Result<UdpSocket> {
    let socket = UdpSocket::from(bound(at, libc::SOCK_DGRAM, mode)?);
    let (level, option) = if at.is_ipv4() {
        (libc::SOL_IP, libc::IP_RECVORIGDSTADDR)
    } else {
        (libc::SOL_IPV6, libc::IPV6_RECVORIGDSTADDR)
    };
    set_option(socket.as_raw_fd(), level, option)?;
    Ok(socket)
}

fn udp_serve(case: Case, mode: SocketMode, deadline: &Deadline) -> io::Result<()> {
    let socket = udp_socket(case.address, mode)?;
    udp_serve_on(case, mode, deadline, socket)
}

fn udp_serve_on(case: Case, mode: SocketMode, deadline: &Deadline, socket: UdpSocket) -> io::Result<()> {
    event(
        State::Listening,
        format_args!("transport=udp local={} mode={mode:?}", socket.local_addr()?),
    )?;
    let (got, peer, original) = udp_receive(&socket, &case.received, deadline)?;
    observed(&got, socket.local_addr()?, peer, original)?;
    let n = deadline.io(socket.as_raw_fd(), libc::POLLOUT, || {
        socket.send_to(&case.sent, peer)
    })?;
    if n != case.sent.len() {
        return Err(invalid("UDP send was partial"));
    }
    event(State::Sent, format_args!("transport=udp bytes={n}"))?;
    event(State::Matched, format_args!("transport=udp"))
}

fn paired(mut client: Case, deadline: Deadline) -> io::Result<()> {
    let copied = |bytes: &[u8]| -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        out.try_reserve_exact(bytes.len()).map_err(io::Error::other)?;
        out.extend_from_slice(bytes);
        Ok(out)
    };
    let server = Case {
        transport: client.transport,
        address: client.address,
        sent: copied(&client.received)?,
        received: copied(&client.sent)?,
        within: client.within,
    };
    // eventfd's uint64 counter wakes the owned server on a failed client, even while
    // that server is blocked in poll. It closes only after the worker has joined.
    // SAFETY: eventfd returns one descriptor, owned immediately below.
    let cancel = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if cancel < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: cancel is the new uniquely owned eventfd.
    let cancel = unsafe { OwnedFd::from_raw_fd(cancel) };
    let server_deadline = Deadline {
        at: deadline.at,
        cancel: Some(cancel.as_raw_fd()),
    };
    let (address, worker) = match client.transport {
        Transport::Tcp => {
            let socket = bound(client.address, libc::SOCK_STREAM, SocketMode::Native)?;
            // SAFETY: listen admits the single connection in this paired fixture.
            if unsafe { libc::listen(socket.as_raw_fd(), 1) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let listener = TcpListener::from(socket);
            let address = listener.local_addr()?;
            let worker = std::thread::Builder::new()
                .spawn(move || tcp_serve_on(server, SocketMode::Native, &server_deadline, listener))?;
            (address, worker)
        }
        Transport::Udp | Transport::UdpConnected => {
            let socket = udp_socket(client.address, SocketMode::Native)?;
            let address = socket.local_addr()?;
            let worker = std::thread::Builder::new()
                .spawn(move || udp_serve_on(server, SocketMode::Native, &server_deadline, socket))?;
            (address, worker)
        }
    };
    client.address = address;
    let result = match client.transport {
        Transport::Tcp => tcp_dial(client, &deadline),
        Transport::Udp | Transport::UdpConnected => udp_dial(client, &deadline),
    };
    if result.is_err() {
        let word = 1u64;
        loop {
            // SAFETY: eventfd takes exactly one uint64; only this caller writes once.
            let wrote =
                unsafe { libc::write(cancel.as_raw_fd(), (&raw const word).cast(), size_of_val(&word)) };
            if wrote >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
    let served = worker
        .join()
        .map_err(|_| io::Error::other("paired probe worker failed"))?;
    result.and(served)
}

fn udp_dial(case: Case, deadline: &Deadline) -> io::Result<()> {
    let local = if case.address.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = udp_socket(local, SocketMode::Native)?;
    let n = match case.transport {
        Transport::UdpConnected => {
            socket.connect(case.address)?;
            deadline.io(socket.as_raw_fd(), libc::POLLOUT, || socket.send(&case.sent))?
        }
        Transport::Udp => deadline.io(socket.as_raw_fd(), libc::POLLOUT, || {
            socket.send_to(&case.sent, case.address)
        })?,
        Transport::Tcp => return Err(invalid("expected UDP")),
    };
    if n != case.sent.len() {
        return Err(invalid("UDP send was partial"));
    }
    event(
        State::Sent,
        format_args!(
            "transport={:?} local={} destination={} bytes={n}",
            case.transport,
            socket.local_addr()?,
            case.address
        ),
    )?;
    let (got, peer, original) = udp_receive(&socket, &case.received, deadline)?;
    observed(&got, socket.local_addr()?, peer, original)?;
    if peer != case.address {
        return Err(invalid("UDP reply source differs from requested destination"));
    }
    event(State::Matched, format_args!("transport={:?}", case.transport))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UdpMetadata {
    peer: SocketAddr,
    original: Option<SocketAddr>,
}

#[derive(Debug)]
enum UdpMetadataError {
    Peer(io::Error),
    UnexpectedRecord,
    HeaderOverflow,
    CapacityOverflow,
    ShortOriginal,
    Original(io::Error),
}
impl UdpMetadataError {
    fn tag(&self) -> &'static str {
        match self {
            Self::Peer(_) => "PeerAddress",
            Self::UnexpectedRecord => "UnexpectedRecord",
            Self::HeaderOverflow => "HeaderOverflow",
            Self::CapacityOverflow => "CapacityOverflow",
            Self::ShortOriginal => "ShortOriginal",
            Self::Original(_) => "OriginalAddress",
        }
    }
    fn into_io(self) -> io::Error {
        match self {
            Self::Peer(e) | Self::Original(e) => e,
            Self::UnexpectedRecord => invalid("unexpected UDP ancillary record"),
            Self::HeaderOverflow => invalid("ancillary header length overflows"),
            Self::CapacityOverflow => invalid("ancillary capacity overflows"),
            Self::ShortOriginal => invalid("short UDP original destination record"),
        }
    }
}

struct UdpArrival {
    reported: usize,
    flags: i32,
    copied: Vec<u8>,
    metadata: Result<UdpMetadata, UdpMetadataError>,
}
impl UdpArrival {
    fn payload_matches(&self, expected: &[u8]) -> bool {
        self.reported == expected.len()
            && self.flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
            && self.copied == expected
    }
}

fn udp_receive(
    socket: &UdpSocket,
    expected: &[u8],
    deadline: &Deadline,
) -> io::Result<(Vec<u8>, SocketAddr, Option<SocketAddr>)> {
    let arrival = udp_arrival(socket, expected.len(), deadline)?;
    // Preserve the native probe's strict validation order and error contract.
    if arrival.reported != expected.len() || arrival.flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(invalid(
            "UDP datagram or ancillary data was truncated/has a different length",
        ));
    }
    if arrival.copied != expected {
        return Err(invalid("UDP datagram differs"));
    }
    let metadata = arrival.metadata.map_err(UdpMetadataError::into_io)?;
    Ok((arrival.copied, metadata.peer, metadata.original))
}

struct UdpReceived {
    reported: usize,
    flags: i32,
    copied: usize,
    metadata: Result<UdpMetadata, UdpMetadataError>,
}
struct UdpView<'a> {
    reported: usize,
    flags: i32,
    copied: &'a [u8],
    metadata: &'a Result<UdpMetadata, UdpMetadataError>,
}
impl UdpArrival {
    fn view(&self) -> UdpView<'_> {
        UdpView {
            reported: self.reported,
            flags: self.flags,
            copied: &self.copied,
            metadata: &self.metadata,
        }
    }
}
impl UdpReceived {
    fn view<'a>(&'a self, bytes: &'a [u8]) -> io::Result<UdpView<'a>> {
        Ok(UdpView {
            reported: self.reported,
            flags: self.flags,
            copied: bytes
                .get(..self.copied)
                .ok_or_else(|| invalid("received extent exceeds supplied buffer"))?,
            metadata: &self.metadata,
        })
    }
}
fn udp_arrival(socket: &UdpSocket, capacity: usize, deadline: &Deadline) -> io::Result<UdpArrival> {
    let mut got = buffer(capacity)?;
    let received = udp_arrival_into(socket, &mut got, deadline)?;
    got.truncate(received.copied);
    Ok(UdpArrival {
        reported: received.reported,
        flags: received.flags,
        copied: got,
        metadata: received.metadata,
    })
}
// Both owning native calls and the finite relay use this ONE recvmsg/cmsg decoder.
// Borrowed initialized storage stays owned by the caller across readiness retries.
fn udp_arrival_into(socket: &UdpSocket, got: &mut [u8], deadline: &Deadline) -> io::Result<UdpReceived> {
    // Linux CMSG_SPACE aligns both header and data to sizeof(size_t).
    const WORDS: usize = size_of::<libc::cmsghdr>().div_ceil(size_of::<usize>())
        + size_of::<libc::sockaddr_in6>().div_ceil(size_of::<usize>());
    let mut control = [0usize; WORDS];
    // SAFETY: all-zero sockaddr_storage/msghdr are valid storage/output parameters.
    let mut peer: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut vector = libc::iovec {
        iov_base: got.as_mut_ptr().cast(),
        iov_len: got.len(),
    };
    // SAFETY: all-zero msghdr is initialized with owned buffers below.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_name = (&raw mut peer).cast();
    message.msg_namelen = size_of::<libc::sockaddr_storage>() as _;
    message.msg_iov = &raw mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // libc exposes socklen_t on musl and size_t on GNU. The same checked
    // conversion is intentionally an identity conversion on the latter ABI.
    #[allow(clippy::useless_conversion)]
    {
        message.msg_controllen = size_of_val(&control)
            .try_into()
            .map_err(|_| invalid("ancillary capacity exceeds ABI field"))?;
    }
    let n = deadline.io(socket.as_raw_fd(), libc::POLLIN, || {
        // SAFETY: every msg buffer above lives through recvmsg; MSG_TRUNC returns the
        // actual datagram length, including when an empty/short buffer truncates it.
        let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &raw mut message, libc::MSG_TRUNC) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    })?;
    // An Ok recvmsg consumed one datagram even when validation later fails.
    Ok(UdpReceived {
        reported: n,
        flags: message.msg_flags,
        copied: n.min(got.len()),
        metadata: udp_metadata(&peer, &message),
    })
}

fn udp_metadata(
    peer: &libc::sockaddr_storage,
    message: &libc::msghdr,
) -> Result<UdpMetadata, UdpMetadataError> {
    let peer = address(peer, message.msg_namelen as usize).map_err(UdpMetadataError::Peer)?;
    // SAFETY: CMSG_FIRSTHDR returns a header inside the control buffer, or null.
    let header = unsafe { libc::CMSG_FIRSTHDR(message) };
    let original = if header.is_null() {
        None
    } else {
        // SAFETY: FIRSTHDR established the header lies within msg_control.
        let h = unsafe { &*header };
        let family = match (h.cmsg_level, h.cmsg_type) {
            (libc::SOL_IP, libc::IP_ORIGDSTADDR) => libc::AF_INET,
            (libc::SOL_IPV6, libc::IPV6_ORIGDSTADDR) => libc::AF_INET6,
            _ => return Err(UdpMetadataError::UnexpectedRecord),
        };
        let bytes = if family == libc::AF_INET {
            size_of::<libc::sockaddr_in>()
        } else {
            size_of::<libc::sockaddr_in6>()
        };
        // Compare in the buffer's address-sized type on both libc ABIs.
        #[allow(clippy::useless_conversion)]
        let (header_len, capacity) = (
            usize::try_from(h.cmsg_len).map_err(|_| UdpMetadataError::HeaderOverflow)?,
            usize::try_from(message.msg_controllen).map_err(|_| UdpMetadataError::CapacityOverflow)?,
        );
        // SAFETY: CMSG_LEN computes the ABI header plus requested data length.
        if header_len < unsafe { libc::CMSG_LEN(bytes as _) } as usize || header_len > capacity {
            return Err(UdpMetadataError::ShortOriginal);
        }
        // SAFETY: header/data lengths above were checked; destination is initialized
        // storage of the largest sockaddr, filled with exactly the family's bytes.
        let mut destination: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        // SAFETY: source and destination are disjoint valid ranges of `bytes` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(libc::CMSG_DATA(header), (&raw mut destination).cast(), bytes)
        };
        Some(address(&destination, bytes).map_err(UdpMetadataError::Original)?)
    };
    Ok(UdpMetadata { peer, original })
}

fn address(storage: &libc::sockaddr_storage, len: usize) -> io::Result<SocketAddr> {
    match i32::from(storage.ss_family) {
        libc::AF_INET if len >= size_of::<libc::sockaddr_in>() => {
            // SAFETY: the family and length identify a complete IPv4 sockaddr.
            let at = unsafe { &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            Ok(SocketAddr::from((
                Ipv4Addr::from(at.sin_addr.s_addr.to_ne_bytes()),
                u16::from_be(at.sin_port),
            )))
        }
        libc::AF_INET6 if len >= size_of::<libc::sockaddr_in6>() => {
            // SAFETY: the family and length identify a complete IPv6 sockaddr.
            let at = unsafe { &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                Ipv6Addr::from(at.sin6_addr.s6_addr),
                u16::from_be(at.sin6_port),
                u32::from_be(at.sin6_flowinfo),
                at.sin6_scope_id,
            )))
        }
        _ => Err(invalid("short or unsupported socket address")),
    }
}

fn capabilities(config: Option<&str>) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-network capabilities kernel={} netns={}",
        std::fs::read_to_string("/proc/sys/kernel/osrelease")?.trim(),
        std::fs::read_link("/proc/self/ns/net")?.display()
    )?;
    for (name, at) in [
        ("ipv4", SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
        ("ipv6", SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))),
    ] {
        match bound(at, libc::SOCK_STREAM, SocketMode::Transparent) {
            Ok(_) => writeln!(
                out,
                "managed-network capability {name}-transparent-bind=accepted nft-hook=unverified"
            )?,
            Err(e) => writeln!(
                out,
                "managed-network capability {name}-transparent-bind=refused errno={:?} kind={:?}",
                e.raw_os_error(),
                e.kind()
            )?,
        }
    }
    match config {
        Some(path) => {
            let mut file = std::fs::File::open(path)?;
            let len = usize::try_from(file.metadata()?.len())
                .map_err(|_| invalid("config length overflows address space"))?;
            let mut text = buffer(len)?;
            file.read_exact(&mut text)?;
            let text = std::str::from_utf8(&text).map_err(|_| invalid("config is not UTF8"))?;
            for name in CONFIG {
                let status = config_status(text, name);
                writeln!(out, "managed-network config {name}={status}")?;
            }
        }
        None => writeln!(
            out,
            "managed-network config=unverified provide-exact-built-config"
        )?,
    }
    out.flush()
}

fn config_status<'a>(text: &'a str, name: &str) -> &'a str {
    for line in text.lines() {
        if let Some(value) = line.strip_prefix(name).and_then(|s| s.strip_prefix('=')) {
            return match value {
                "y" => "y",
                "m" => "m",
                "n" => "n",
                _ => "invalid",
            };
        }
        if line.strip_prefix("# ").and_then(|s| s.strip_prefix(name)) == Some(" is not set") {
            return "n";
        }
    }
    "absent"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_receive_view_preserves_reported_extent_and_metadata_error() {
        let received = UdpReceived {
            reported: 4,
            flags: libc::MSG_TRUNC,
            copied: 2,
            metadata: Err(UdpMetadataError::UnexpectedRecord),
        };
        let storage = [0, 255, 99];
        let view = received.view(&storage).unwrap();
        assert_eq!(view.reported, 4);
        assert_eq!(view.copied, [0, 255]);
        assert_eq!(view.flags, libc::MSG_TRUNC);
        assert!(matches!(view.metadata, Err(UdpMetadataError::UnexpectedRecord)));
        assert!(matches!(received.view(&[]), Err(error) if error.kind() == io::ErrorKind::InvalidInput));
    }
    #[test]
    fn udp_interception_parser_has_explicit_origins() {
        let args = |words: &[&str]| words.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        for protocol in ["intercept-udp4", "intercept-udp6"] {
            for origin in ["local", "forward", "local-loopback", "local-self"] {
                for mode in ["connected", "unconnected"] {
                    assert!(matches!(
                        parse(&args(&[protocol, origin, mode, "1000", "1", "1000", "100"])).unwrap(),
                        Probe::InterceptUdp(_)
                    ));
                }
            }
        }
        assert!(
            parse(&args(&[
                "intercept-udp4",
                "forward-relay-absent",
                "connected",
                "1000",
                "1",
                "1000",
                "100"
            ]))
            .is_err()
        );
    }

    #[test]
    fn hex_is_exact_and_rejects_malformed_inputs() {
        assert_eq!(unhex("00ff0a8041").unwrap(), [0, 255, 10, 128, 65]);
        assert!(unhex("f").is_err());
        assert!(unhex("gg").is_err());
        assert!(unhex("é").is_err());
        assert!(unhex("").unwrap().is_empty());
    }

    #[test]
    fn probe_commands_are_explicit() {
        let args = |words: &[&str]| words.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(matches!(
            parse(&args(&["capabilities"])).unwrap(),
            Probe::Capabilities(None)
        ));
        assert!(matches!(
            parse(&args(&[
                "serve",
                "tcp",
                "127.0.0.1:0",
                "",
                "00ff",
                "0",
                "transparent"
            ]))
            .unwrap(),
            Probe::Serve(_, SocketMode::Transparent)
        ));
        assert!(
            parse(&args(&[
                "serve",
                "udp-connected",
                "127.0.0.1:0",
                "",
                "",
                "1",
                "native"
            ]))
            .is_err()
        );
        assert!(parse(&args(&["dial", "tcp", "host:1", "", "", "1", "native"])).is_err());
    }

    #[test]
    fn config_evidence_does_not_treat_missing_symbols_as_enabled() {
        let text = "CONFIG_NFT_TPROXY=y\n# CONFIG_NFT_SOCKET is not set\nCONFIG_NF_TABLES_IPV4=m\nCONFIG_CGROUP_BPF=oops\n";
        assert_eq!(config_status(text, "CONFIG_NFT_TPROXY"), "y");
        assert_eq!(config_status(text, "CONFIG_NFT_SOCKET"), "n");
        assert_eq!(config_status(text, "CONFIG_NF_TABLES_IPV4"), "m");
        assert_eq!(config_status(text, "CONFIG_CGROUP_BPF"), "invalid");
        assert_eq!(config_status(text, "CONFIG_NF_TPROXY_IPV6"), "absent");
    }

    #[test]
    fn a_zero_deadline_does_not_attempt_io() {
        assert_eq!(
            Deadline::new(Duration::ZERO).err().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        let deadline = Deadline {
            at: Instant::now(),
            cancel: None,
        };
        let mut attempted = false;
        let result = deadline.io(-1, libc::POLLIN, || {
            attempted = true;
            Ok(())
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(!attempted);
    }

    #[test]
    fn an_unrepresentable_deadline_is_refused() {
        assert_eq!(
            Deadline::new(Duration::MAX).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn interception_commands_keep_explicit_ip_families() {
        for protocol in ["intercept-tcp4", "intercept-tcp6"] {
            let args: Vec<_> = [protocol, "forward-relay-absent", "1000", "1", "1000", "100"]
                .into_iter()
                .map(str::to_owned)
                .collect();
            assert!(matches!(parse(&args).unwrap(), Probe::Intercept(_)));
        }
        let args: Vec<_> = ["intercept-tcp5", "local", "1000", "1", "1000", "100"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert!(parse(&args).is_err());
    }
}
