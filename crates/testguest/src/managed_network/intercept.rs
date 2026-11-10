//! Coordinator-executed IPv4 TCP TPROXY experiment, never product installation.
//! Linux v6.18 nft_tproxy_tcp.sh's local/forward distinction; all administration
//! happens in fresh guest namespaces owned solely by this finite fixture.

use super::*;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::thread::JoinHandle;

mod wire;
use wire::Netlink;

// RFC5737 documentation networks, with /30 point-to-point fixture links. These
// are semantic test addresses, not allocated production subnets or runtime policy.
const ROUTER_BACKEND: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const BACKEND: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
const ROUTER_CLIENT: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 1);
const CLIENT: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 2);
const SENT: &[u8] = b"REQUEST\0\xff";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Local,
    Forward,
    LocalLoopback,
    LocalSelf,
}

#[derive(Debug)]
pub(super) struct Inputs {
    origin: Origin,
    within: Duration,
    mark: u32,
    table: u32,
    priority: u32,
}

impl Inputs {
    pub(super) fn parse(
        origin: &str,
        millis: &str,
        mark: &str,
        table: &str,
        priority: &str,
    ) -> io::Result<Self> {
        let number = |s: &str| {
            s.parse::<u32>()
                .map_err(|_| invalid("expected unsigned decimal ABI-width integer"))
        };
        let mark = number(mark)?;
        let table = number(table)?;
        let priority = number(priority)?;
        // fib_rules.c's default local/main/default priorities are0/32766/32767;
        // rtnetlink.h reserves tables0/253/254/255. Do not replace these objects.
        if mark == 0 || matches!(table, 0 | 253..=255) || !(1..32766).contains(&priority) {
            return Err(invalid("mark/table/rule priority conflicts with default routing"));
        }
        let within = Duration::from_millis(millis.parse().map_err(|_| invalid("deadline is milliseconds"))?);
        Deadline::new(within)?;
        Ok(Self {
            origin: match origin {
                "local" => Origin::Local,
                "forward" => Origin::Forward,
                "local-loopback" => Origin::LocalLoopback,
                "local-self" => Origin::LocalSelf,
                _ => {
                    return Err(invalid(
                        "origin must be local, forward, local-loopback or local-self",
                    ));
                }
            },
            within,
            mark,
            table,
            priority,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    Backend,
    Relay,
}
impl Reply {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Backend => b"BACKEND\0\xff",
            Self::Relay => b"RELAY\0\x80",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Namespaces,
    Target,
    Baseline,
    Installed,
    Intercepted,
    Joined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tuple {
    local: SocketAddr,
    peer: SocketAddr,
}

#[derive(Debug, Default)]
struct Report {
    accepted: usize,
    last: Option<Tuple>,
}

struct Namespace {
    handle: File,
    route: Netlink,
}
impl Namespace {
    fn new(deadline: &Deadline) -> io::Result<Self> {
        in_namespace(None, || {
            let handle = File::open("/proc/thread-self/ns/net")?;
            let route = Netlink::open(libc::NETLINK_ROUTE)?;
            route.link_up(interface(c"lo")?, deadline)?;
            Ok(Self { handle, route })
        })
    }
    fn inode(&self) -> io::Result<u64> {
        Ok(self.handle.metadata()?.ino())
    }
}

fn in_namespace<T: Send>(
    namespace: Option<&File>,
    f: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new().spawn_scoped(scope, move || {
            // SAFETY: this new dedicated thread alone changes its network namespace;
            // the creator stays untouched. None always creates a fresh namespace.
            let rc = unsafe {
                match namespace {
                    Some(ns) => libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET),
                    None => libc::unshare(libc::CLONE_NEWNET),
                }
            };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            f()
        })?;
        worker
            .join()
            .map_err(|_| io::Error::other("namespace worker panicked"))?
    })
}

fn interface(name: &std::ffi::CStr) -> io::Result<u32> {
    // SAFETY: name is a NUL-terminated fixture interface name.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(index)
    }
}

fn endpoint(
    namespace: &Namespace,
    name: &'static std::ffi::CStr,
    ip: Ipv4Addr,
    gateway: Ipv4Addr,
    deadline: &Deadline,
) -> io::Result<()> {
    in_namespace(Some(&namespace.handle), || {
        let index = interface(name)?;
        namespace.route.link_up(index, deadline)?;
        namespace.route.address(index, ip, deadline)?;
        namespace.route.default_route(index, gateway, deadline)
    })
}

fn listen(at: SocketAddr, mode: SocketMode) -> io::Result<TcpListener> {
    let socket = bound(at, libc::SOCK_STREAM, mode)?;
    // SAFETY: this owned stream socket admits the fixture's connections only.
    if unsafe { libc::listen(socket.as_raw_fd(), 1) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(TcpListener::from(socket))
}

/// Two fixed workers at most. On every terminal path Drop wakes accept/read/write
/// and joins; the original absolute deadline bounds even a missed cancellation.
struct Worker {
    cancel: OwnedFd,
    state: WorkerState,
}

enum WorkerState {
    Running(JoinHandle<io::Result<Report>>),
    Joined,
}
impl Worker {
    fn start(listener: TcpListener, reply: Reply, deadline: &Deadline) -> io::Result<Self> {
        // SAFETY: eventfd returns a new owned descriptor.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is the uniquely owned newly created eventfd.
        let cancel = unsafe { OwnedFd::from_raw_fd(fd) };
        let deadline = Deadline {
            at: deadline.at,
            cancel: Some(cancel.as_raw_fd()),
        };
        let thread = std::thread::Builder::new().spawn(move || {
            let mut report = Report::default();
            loop {
                let (mut stream, peer) =
                    match deadline.io(listener.as_raw_fd(), libc::POLLIN, || listener.accept()) {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => return Ok(report),
                        Err(e) => return Err(e),
                    };
                report.accepted = report
                    .accepted
                    .checked_add(1)
                    .ok_or_else(|| invalid("accept count overflow"))?;
                // Baseline+intercept are the only two calls; no open-ended server.
                if report.accepted > 2 {
                    return Err(invalid("unexpected extra fixture connection"));
                }
                stream.set_nonblocking(true)?;
                let tuple = Tuple {
                    local: stream.local_addr()?,
                    peer,
                };
                report.last = Some(tuple);
                let received = tcp_receive(&mut stream, SENT, &deadline)?;
                observed(&received, tuple.local, tuple.peer, Some(tuple.local))?;
                let mut out = io::stdout().lock();
                writeln!(out, "managed-intercept {{\"type\":\"ServerObservation\",\"role\":\"{reply:?}\",\"local\":\"{}\",\"peer\":\"{}\"}}", tuple.local, tuple.peer)?;
                out.flush()?;
                drop(out);
                tcp_send(&mut stream, reply.bytes(), &deadline)?;
                stream.shutdown(Shutdown::Write)?;
                if reply == Reply::Relay {
                    return Ok(report);
                }
            }
        })?;
        Ok(Self {
            cancel,
            state: WorkerState::Running(thread),
        })
    }
    fn wake(&self) {
        let word = 1u64;
        // SAFETY: eventfd takes one uint64. Even a failed/interrupted wake cannot
        // extend the worker beyond its original absolute deadline.
        let _ = unsafe {
            libc::write(
                self.cancel.as_raw_fd(),
                (&raw const word).cast(),
                size_of_val(&word),
            )
        };
    }
    fn finish(&mut self) -> io::Result<Report> {
        self.wake();
        match std::mem::replace(&mut self.state, WorkerState::Joined) {
            WorkerState::Running(thread) => thread
                .join()
                .map_err(|_| io::Error::other("socket worker panicked"))?,
            WorkerState::Joined => Err(invalid("worker already joined")),
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.wake();
        if let WorkerState::Running(thread) = std::mem::replace(&mut self.state, WorkerState::Joined) {
            let _ = thread.join();
        }
    }
}

fn client(
    namespace: &Namespace,
    destination: SocketAddr,
    reply: Reply,
    deadline: &Deadline,
) -> io::Result<Tuple> {
    in_namespace(Some(&namespace.handle), || {
        let mut stream = TcpStream::connect_timeout(&destination, deadline.left()?)?;
        stream.set_nonblocking(true)?;
        let tuple = Tuple {
            local: stream.local_addr()?,
            peer: stream.peer_addr()?,
        };
        tcp_send(&mut stream, SENT, deadline)?;
        stream.shutdown(Shutdown::Write)?;
        let got = tcp_receive(&mut stream, reply.bytes(), deadline)?;
        observed(&got, tuple.local, tuple.peer, None)?;
        let mut out = io::stdout().lock();
        write!(
            out,
            "managed-intercept {{\"type\":\"ClientObservation\",\"reply\":\"{reply:?}\",\"local\":\"{}\",\"peer\":\"{}\",\"hex\":\"",
            tuple.local, tuple.peer
        )?;
        for byte in got {
            write!(out, "{byte:02x}")?;
        }
        writeln!(out, "\"}}")?;
        out.flush()?;
        Ok(tuple)
    })
}

pub(super) fn run(input: Inputs) -> io::Result<()> {
    // Direct testguest PID1 only; even there Namespace::new unshares before any
    // administrative operation. No host/current namespace is ever configured here.
    if std::process::id() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "interception experiment requires direct guest PID1",
        ));
    }
    let deadline = Deadline::new(input.within)?;
    let router = Namespace::new(&deadline)?;
    let backend = Namespace::new(&deadline)?;
    let client_ns = Namespace::new(&deadline)?;
    {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "managed-intercept {{\"type\":\"{0:?}\",\"origin\":\"{1:?}\",\"router_inode\":{2},\"backend_inode\":{3},\"client_inode\":{4},\"mark\":{5},\"table\":{6},\"priority\":{7}}}",
            Phase::Namespaces,
            input.origin,
            router.inode()?,
            backend.inode()?,
            client_ns.inode()?,
            input.mark,
            input.table,
            input.priority
        )?;
        out.flush()?;
    }
    in_namespace(Some(&router.handle), || {
        // Sysctls resolve this dedicated thread's NEW namespace. Disable reverse
        // path filtering before creating links, as local-route reinjection needs.
        std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1")?;
        std::fs::write("/proc/sys/net/ipv4/conf/all/rp_filter", b"0")?;
        std::fs::write("/proc/sys/net/ipv4/conf/default/rp_filter", b"0")?;
        router.route.veth(
            b"to-backend\0",
            b"backend0\0",
            backend.handle.as_raw_fd(),
            &deadline,
        )?;
        router.route.veth(
            b"to-client\0",
            b"client0\0",
            client_ns.handle.as_raw_fd(),
            &deadline,
        )?;
        for (name, ip) in [(c"to-backend", ROUTER_BACKEND), (c"to-client", ROUTER_CLIENT)] {
            let index = interface(name)?;
            router.route.link_up(index, &deadline)?;
            router.route.address(index, ip, &deadline)?;
        }
        Ok(())
    })?;
    endpoint(&backend, c"backend0", BACKEND, ROUTER_BACKEND, &deadline)?;
    endpoint(&client_ns, c"client0", CLIENT, ROUTER_CLIENT, &deadline)?;
    let (backend_namespace, backend_ip) = match input.origin {
        Origin::Local | Origin::Forward => (&backend, BACKEND),
        Origin::LocalLoopback => (&router, Ipv4Addr::LOCALHOST),
        Origin::LocalSelf => (&router, ROUTER_BACKEND),
    };
    let origin = match input.origin {
        Origin::Forward => &client_ns,
        Origin::Local | Origin::LocalLoopback | Origin::LocalSelf => &router,
    };
    let backend_listener = in_namespace(Some(&backend_namespace.handle), || {
        listen(SocketAddr::from((backend_ip, 0)), SocketMode::Native)
    })?;
    let destination = backend_listener.local_addr()?;
    let relay_listener = in_namespace(Some(&router.handle), || {
        listen(
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            SocketMode::Transparent,
        )
    })?;
    let relay_port = relay_listener.local_addr()?.port();
    {
        // Namespaces above names the fixed topology's three objects. Target names
        // the actual roles, which share the router in the same-namespace cases.
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "managed-intercept {{\"type\":\"{:?}\",\"origin\":\"{:?}\",\"destination\":\"{destination}\",\"client_inode\":{},\"backend_inode\":{},\"relay_inode\":{}}}",
            Phase::Target,
            input.origin,
            origin.inode()?,
            backend_namespace.inode()?,
            router.inode()?
        )?;
        out.flush()?;
    }
    let mut backend_worker = Worker::start(backend_listener, Reply::Backend, &deadline)?;
    let baseline = client(origin, destination, Reply::Backend, &deadline)?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"{:?}\",\"destination\":\"{destination}\",\"relay_port\":{relay_port}}}",
        Phase::Baseline
    )?;
    out.flush()?;
    drop(out);
    in_namespace(Some(&router.handle), || {
        router
            .route
            .policy(interface(c"lo")?, &input, destination, &deadline)?;
        Netlink::open(libc::NETLINK_NETFILTER)?.interception(&input, destination, relay_port, &deadline)
    })?;
    let mut out = io::stdout().lock();
    writeln!(out, "managed-intercept {{\"type\":\"{:?}\"}}", Phase::Installed)?;
    out.flush()?;
    drop(out);
    let mut relay_worker = Worker::start(relay_listener, Reply::Relay, &deadline)?;
    let result = client(origin, destination, Reply::Relay, &deadline);
    // Join both workers before propagating even a failed/mutated client result.
    let relay_result = relay_worker.finish();
    let backend_result = backend_worker.finish();
    let relay_report = relay_result?;
    let backend_report = backend_result?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"{:?}\",\"backend_accepted\":{},\"relay_accepted\":{},\"joined\":true}}",
        Phase::Joined,
        backend_report.accepted,
        relay_report.accepted
    )?;
    out.flush()?;
    drop(out);
    let client_tuple = result?;
    let accepted = relay_report
        .last
        .ok_or_else(|| invalid("relay accepted no connection"))?;
    if accepted.local != destination
        || accepted.peer != client_tuple.local
        || client_tuple.peer != destination
        || relay_report.accepted != 1
        || backend_report.accepted != 1
        || backend_report.last
            != Some(Tuple {
                local: destination,
                peer: baseline.local,
            })
    {
        return Err(invalid("interception original tuple or acceptance counts differ"));
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"{:?}\",\"original\":\"{}\",\"peer\":\"{}\",\"client_local\":\"{}\",\"client_peer\":\"{}\"}}",
        Phase::Intercepted,
        accepted.local,
        accepted.peer,
        client_tuple.local,
        client_tuple.peer
    )?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inputs_refuse_conflicting_or_out_of_width_routing_objects() {
        for (mark, table, priority) in [
            ("0", "100", "100"),
            ("1", "254", "100"),
            ("1", "0", "100"),
            ("1", "100", "0"),
            ("1", "100", "32766"),
            ("4294967296", "100", "100"),
        ] {
            assert!(Inputs::parse("local", "1000", mark, table, priority).is_err());
        }
        assert!(Inputs::parse("local", "0", "1", "100", "100").is_err());
        assert!(Inputs::parse("ipv6", "1000", "1", "100", "100").is_err());
        assert_eq!(
            Inputs::parse("forward", "1000", "1", "1000", "100")
                .unwrap()
                .origin,
            Origin::Forward
        );
        assert_ne!(Reply::Backend.bytes(), Reply::Relay.bytes());
        for (name, origin) in [
            ("local", Origin::Local),
            ("forward", Origin::Forward),
            ("local-loopback", Origin::LocalLoopback),
            ("local-self", Origin::LocalSelf),
        ] {
            assert_eq!(
                Inputs::parse(name, "1000", "1", "1000", "100").unwrap().origin,
                origin
            );
        }
    }
}
