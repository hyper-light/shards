//! Coordinator-executed IPv4/IPv6 TCP TPROXY experiment, never product installation.
//! Linux v6.18 nft_tproxy_tcp.sh's local/forward distinction; all administration
//! happens in fresh guest namespaces owned solely by this finite fixture.

use super::*;
use std::fs::File;
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::thread::JoinHandle;

pub(super) mod sock_addr;
pub(super) mod tcp_forward;
pub(super) mod udp;
mod wire;
use wire::Netlink;

#[path = "../../../init/src/relay.rs"]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the same module's process-owned engine policy is consumed by init"
    )
)]
pub(super) mod shared;

// RFC5737 documentation networks, with /30 point-to-point fixture links. These
// are semantic test addresses, not allocated production subnets or runtime policy.
const ROUTER_BACKEND: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const BACKEND: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
const ROUTER_CLIENT: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 1);
const CLIENT: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 2);
// RFC3849 documentation-only subnets; /64 matches the pinned selftest's
// IPv6 link layout. These are fixture identities, not runtime address policy.
const ROUTER_BACKEND6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1);
const BACKEND6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2);
const ROUTER_CLIENT6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 2, 0, 0, 0, 0, 1);
const CLIENT6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 2, 0, 0, 0, 0, 2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    Ipv4,
    Ipv6,
}
impl Family {
    fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(_) => Self::Ipv4,
            IpAddr::V6(_) => Self::Ipv6,
        }
    }
}

const SENT: &[u8] = b"REQUEST\0\xff";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Local,
    Forward,
    LocalLoopback,
    LocalSelf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelayMode {
    Present,
    Absent,
}

#[derive(Debug)]
pub(super) struct Inputs {
    family: Family,
    origin: Origin,
    relay: RelayMode,
    within: Duration,
    mark: u32,
    table: u32,
    priority: u32,
}

impl Inputs {
    pub(super) fn parse(
        family: Family,
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
            family,
            origin: match origin {
                "local" => Origin::Local,
                "forward" | "forward-relay-absent" => Origin::Forward,
                "local-loopback" => Origin::LocalLoopback,
                "local-self" => Origin::LocalSelf,
                _ => {
                    return Err(invalid(
                        "expected local, forward, local-loopback, local-self or forward-relay-absent",
                    ));
                }
            },
            relay: if origin == "forward-relay-absent" {
                RelayMode::Absent
            } else {
                RelayMode::Present
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
    RelayListenerClosed,
    RelayAbsentChecked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tuple {
    local: SocketAddr,
    peer: SocketAddr,
}

#[derive(Debug, Default)]
struct Report {
    accepted: usize,
    receipts: usize,
    last: Option<Tuple>,
}

/// Keep actual accepts/complete requests even when later I/O fails.
struct Completion {
    report: Report,
    result: io::Result<()>,
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
    ip: IpAddr,
    gateway: IpAddr,
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

/// Whether a worker's cancellation was requested: its nonblocking eventfd holds the word,
/// consumed here. Deadline::io reports a readable cancel as ConnectionAborted, but the I/O
/// call itself can fail with that kind too (accept(2)'s ECONNABORTED).
fn cancellation_observed(cancel: &impl AsRawFd) -> bool {
    let mut word = 0u64;
    // SAFETY: nonblocking eventfd writes into exactly one initialized uint64.
    unsafe {
        libc::read(cancel.as_raw_fd(), (&raw mut word).cast(), size_of_val(&word))
            == size_of_val(&word) as isize
    }
}

/// Two fixed workers at most. On every terminal path Drop wakes accept/read/write
/// and joins; the original absolute deadline bounds even a missed cancellation.
struct Worker<T = Completion> {
    cancel: OwnedFd,
    state: WorkerState<T>,
}

enum WorkerState<T> {
    Running(JoinHandle<T>),
    Joined,
}
impl Worker {
    fn start(listener: TcpListener, reply: Reply, deadline: &Deadline) -> io::Result<Self> {
        // Before the thread starts: a blocking accept4 never returns to Deadline::io's
        // poll, so neither the wake nor the deadline would reach it.
        listener.set_nonblocking(true)?;
        Self::spawn(deadline, move |deadline| {
            let mut report = Report::default();
            let result = (|| loop {
                let (mut stream, peer) =
                    match deadline.io(listener.as_raw_fd(), libc::POLLIN, || listener.accept()) {
                        Ok(pair) => pair,
                        // A stop only where this worker's cancellation was requested.
                        Err(e)
                            if e.kind() == io::ErrorKind::ConnectionAborted
                                && deadline
                                    .cancel
                                    .is_some_and(|cancel| cancellation_observed(&cancel)) =>
                        {
                            return Ok(());
                        }
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
                report.receipts = report
                    .receipts
                    .checked_add(1)
                    .ok_or_else(|| invalid("receipt count overflow"))?;
                observed(&received, tuple.local, tuple.peer, Some(tuple.local))?;
                let mut out = io::stdout().lock();
                writeln!(
                    out,
                    "managed-intercept {{\"type\":\"ServerObservation\",\"role\":\"{reply:?}\",\"local\":\"{}\",\"peer\":\"{}\"}}",
                    tuple.local, tuple.peer
                )?;
                out.flush()?;
                drop(out);
                tcp_send(&mut stream, reply.bytes(), &deadline)?;
                stream.shutdown(Shutdown::Write)?;
                if reply == Reply::Relay {
                    return Ok(());
                }
            })();
            Completion { report, result }
        })
    }
}

impl<T: Send + 'static> Worker<T> {
    fn spawn(
        deadline: &Deadline,
        operation: impl FnOnce(Deadline) -> T + Send + 'static,
    ) -> io::Result<Self> {
        // SAFETY: eventfd returns a new descriptor, immediately uniquely owned.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful eventfd has transferred sole descriptor ownership.
        let cancel = unsafe { OwnedFd::from_raw_fd(fd) };
        let deadline = Deadline {
            at: deadline.at,
            cancel: Some(cancel.as_raw_fd()),
        };
        let thread = std::thread::Builder::new().spawn(move || operation(deadline))?;
        Ok(Self {
            cancel,
            state: WorkerState::Running(thread),
        })
    }
}

impl<T> Worker<T> {
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
    fn finish(&mut self) -> io::Result<T> {
        self.wake();
        self.join()
    }

    fn join(&mut self) -> io::Result<T> {
        match std::mem::replace(&mut self.state, WorkerState::Joined) {
            WorkerState::Running(thread) => thread
                .join()
                .map_err(|_| io::Error::other("socket worker panicked")),
            WorkerState::Joined => Err(invalid("worker already joined")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientStage {
    BeforeConnect,
    ConnectCall,
    ConnectedIo,
}

struct ClientCompletion {
    stage: ClientStage,
    result: io::Result<Tuple>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptOutcome {
    Success,
    Refused,
    Deadline,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerOutcome {
    Stopped,
    Deadline,
    Error,
}

impl Completion {
    fn outcome(&self) -> WorkerOutcome {
        match &self.result {
            Ok(()) => WorkerOutcome::Stopped,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => WorkerOutcome::Deadline,
            Err(_) => WorkerOutcome::Error,
        }
    }

    fn record_error(&self, role: Reply) -> io::Result<()> {
        if let Err(error) = &self.result {
            let mut out = io::stdout().lock();
            write!(
                out,
                "managed-intercept {{\"type\":\"WorkerError\",\"worker\":\"{role:?}\",\"kind\":\"{:?}\",\"errno\":",
                error.kind()
            )?;
            match error.raw_os_error() {
                Some(errno) => write!(out, "{errno}")?,
                None => write!(out, "null")?,
            }
            writeln!(out, "}}")?;
            out.flush()?;
        }
        Ok(())
    }
}

fn attempt_outcome(client: &ClientCompletion) -> AttemptOutcome {
    match &client.result {
        Ok(_) => AttemptOutcome::Success,
        Err(error)
            if client.stage == ClientStage::ConnectCall
                && error.kind() == io::ErrorKind::ConnectionRefused =>
        {
            AttemptOutcome::Refused
        }
        Err(error) if client.stage == ClientStage::ConnectCall && error.kind() == io::ErrorKind::TimedOut => {
            AttemptOutcome::Deadline
        }
        Err(_) => AttemptOutcome::Error,
    }
}

fn record_attempt(client: &ClientCompletion) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(
        out,
        "managed-intercept {{\"type\":\"ClientOutcome\",\"stage\":\"{:?}\",\"outcome\":\"{:?}\"",
        client.stage,
        attempt_outcome(client)
    )?;
    match &client.result {
        Ok(tuple) => write!(
            out,
            ",\"reply\":\"Backend\",\"local\":\"{}\",\"peer\":\"{}\",\"kind\":null,\"errno\":null",
            tuple.local, tuple.peer
        )?,
        Err(error) => {
            write!(out, ",\"kind\":\"{:?}\",\"errno\":", error.kind())?;
            match error.raw_os_error() {
                Some(errno) => write!(out, "{errno}")?,
                None => write!(out, "null")?,
            }
        }
    }
    writeln!(out, "}}")?;
    out.flush()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AbsenceVerdict {
    ContainedRefusal,
    NoNewBackendEffectBeforeDeadline,
    DirectBackendEffect,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AbsenceWindow {
    AttemptUntilJoin,
    OriginalAbsoluteDeadline,
}

fn absence_verdict(client: &ClientCompletion, backend: &Completion) -> AbsenceVerdict {
    if attempt_outcome(client) == AttemptOutcome::Success {
        return AbsenceVerdict::DirectBackendEffect;
    }
    if backend.report.accepted > 1 || backend.report.receipts > 1 {
        return AbsenceVerdict::DirectBackendEffect;
    }
    if backend.report.accepted != 1 || backend.report.receipts != 1 {
        return AbsenceVerdict::Inconclusive;
    }
    match (attempt_outcome(client), backend.outcome()) {
        (AttemptOutcome::Refused, WorkerOutcome::Stopped) => AbsenceVerdict::ContainedRefusal,
        (AttemptOutcome::Deadline, WorkerOutcome::Stopped | WorkerOutcome::Deadline) => {
            AbsenceVerdict::NoNewBackendEffectBeforeDeadline
        }
        _ => AbsenceVerdict::Inconclusive,
    }
}

fn relay_absent(
    listener: TcpListener,
    backend: &mut Worker,
    origin: &Namespace,
    destination: SocketAddr,
    deadline: &Deadline,
) -> io::Result<()> {
    let port = listener.local_addr()?.port();
    // This is the sole listener owner; no relay worker/accepted socket exists.
    // Close it before client() creates the fresh forward connection.
    drop(listener);
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"{:?}\",\"relay_port\":{port},\"relay_worker\":\"NotStarted\"}}",
        Phase::RelayListenerClosed
    )?;
    out.flush()?;
    drop(out);
    let result = client(origin, destination, Reply::Backend, deadline);
    // Join before any logging/error propagation can leave the owned backend live.
    let completed = backend.finish();
    record_attempt(&result)?;
    let completed = completed?; // Unknown worker state is an error, never zero counts.
    let terminal = completed.outcome();
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"{:?}\",\"backend_accepted\":{},\"backend_receipts\":{},\"backend_terminal\":\"{terminal:?}\",\"relay_accepted\":0,\"relay_receipts\":0,\"relay_worker\":\"NotStarted\",\"joined\":true}}",
        Phase::Joined,
        completed.report.accepted,
        completed.report.receipts
    )?;
    out.flush()?;
    drop(out);
    completed.record_error(Reply::Backend)?;
    let verdict = absence_verdict(&result, &completed);
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"AbsenceVerdict\",\"verdict\":\"{verdict:?}\"}}"
    )?;
    out.flush()?;
    drop(out);
    // Only the checked original-window verdict admits the expected worker
    // deadline. Keep its WorkerError record; every other I/O error remains fatal.
    match completed.result {
        Err(error)
            if verdict == AbsenceVerdict::NoNewBackendEffectBeforeDeadline
                && error.kind() == io::ErrorKind::TimedOut => {}
        result => result?,
    }
    let window = match verdict {
        AbsenceVerdict::DirectBackendEffect => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "absent relay allowed a direct backend connection or complete request",
            ));
        }
        AbsenceVerdict::Inconclusive => {
            result.result?;
            return Err(invalid("relay-absent result is inconclusive"));
        }
        AbsenceVerdict::ContainedRefusal => AbsenceWindow::AttemptUntilJoin,
        AbsenceVerdict::NoNewBackendEffectBeforeDeadline => AbsenceWindow::OriginalAbsoluteDeadline,
    };
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"{:?}\",\"verdict\":\"{verdict:?}\",\"window\":\"{window:?}\",\"after_window\":\"Unverified\",\"no_second_backend_accept_or_receipt\":true}}",
        Phase::RelayAbsentChecked
    )?;
    out.flush()
}
impl<T> Drop for Worker<T> {
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
) -> ClientCompletion {
    let entered = in_namespace(Some(&namespace.handle), || {
        let remaining = match deadline.left() {
            Ok(remaining) => remaining,
            Err(error) => {
                return Ok(ClientCompletion {
                    stage: ClientStage::BeforeConnect,
                    result: Err(error),
                });
            }
        };
        let mut stage = ClientStage::ConnectCall;
        let result = (|| {
            let mut stream = TcpStream::connect_timeout(&destination, remaining)?;
            stage = ClientStage::ConnectedIo;
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
        })();
        Ok(ClientCompletion { stage, result })
    });
    match entered {
        Ok(completed) => completed,
        Err(error) => ClientCompletion {
            stage: ClientStage::BeforeConnect,
            result: Err(error),
        },
    }
}

fn prepare(input: &Inputs, deadline: &Deadline) -> io::Result<[Namespace; 3]> {
    let [router_backend, backend_address, router_client, client_address] = addresses(input.family);
    let router = Namespace::new(deadline)?;
    let backend = Namespace::new(deadline)?;
    let client_ns = Namespace::new(deadline)?;
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
        // Sysctls resolve only this dedicated thread's NEW router namespace.
        match input.family {
            Family::Ipv4 => {
                std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1")?;
                std::fs::write("/proc/sys/net/ipv4/conf/all/rp_filter", b"0")?;
                std::fs::write("/proc/sys/net/ipv4/conf/default/rp_filter", b"0")?;
            }
            Family::Ipv6 => std::fs::write("/proc/sys/net/ipv6/conf/all/forwarding", b"1")?,
        }
        router.route.veth(
            b"to-backend\0",
            b"backend0\0",
            backend.handle.as_raw_fd(),
            deadline,
        )?;
        router.route.veth(
            b"to-client\0",
            b"client0\0",
            client_ns.handle.as_raw_fd(),
            deadline,
        )?;
        for (name, ip) in [(c"to-backend", router_backend), (c"to-client", router_client)] {
            let index = interface(name)?;
            router.route.link_up(index, deadline)?;
            router.route.address(index, ip, deadline)?;
        }
        Ok(())
    })?;
    endpoint(&backend, c"backend0", backend_address, router_backend, deadline)?;
    endpoint(&client_ns, c"client0", client_address, router_client, deadline)?;
    Ok([router, backend, client_ns])
}

fn addresses(family: Family) -> [IpAddr; 4] {
    match family {
        Family::Ipv4 => [
            ROUTER_BACKEND.into(),
            BACKEND.into(),
            ROUTER_CLIENT.into(),
            CLIENT.into(),
        ],
        Family::Ipv6 => [
            ROUTER_BACKEND6.into(),
            BACKEND6.into(),
            ROUTER_CLIENT6.into(),
            CLIENT6.into(),
        ],
    }
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
    let [router_backend, backend_address, _, _] = addresses(input.family);
    let (loopback, unspecified): (IpAddr, IpAddr) = match input.family {
        Family::Ipv4 => (Ipv4Addr::LOCALHOST.into(), Ipv4Addr::UNSPECIFIED.into()),
        Family::Ipv6 => (Ipv6Addr::LOCALHOST.into(), Ipv6Addr::UNSPECIFIED.into()),
    };
    let [router, backend, client_ns] = prepare(&input, &deadline)?;
    let (backend_namespace, backend_ip) = match input.origin {
        Origin::Local | Origin::Forward => (&backend, backend_address),
        Origin::LocalLoopback => (&router, loopback),
        Origin::LocalSelf => (&router, router_backend),
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
        listen(SocketAddr::from((unspecified, 0)), SocketMode::Transparent)
    })?;
    let relay_port = relay_listener.local_addr()?.port();
    {
        // Namespaces above names the fixed topology's three objects. Target names
        // the actual roles, which share the router in the same-namespace cases.
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "managed-intercept {{\"type\":\"{:?}\",\"origin\":\"{:?}\",\"relay_mode\":\"{:?}\",\"family\":\"{:?}\",\"destination\":\"{destination}\",\"client_inode\":{},\"backend_inode\":{},\"relay_inode\":{}}}",
            Phase::Target,
            input.origin,
            input.relay,
            Family::of(destination.ip()),
            origin.inode()?,
            backend_namespace.inode()?,
            router.inode()?
        )?;
        out.flush()?;
    }
    let mut backend_worker = Worker::start(backend_listener, Reply::Backend, &deadline)?;
    let baseline = client(origin, destination, Reply::Backend, &deadline).result?;
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
        Netlink::open(libc::NETLINK_NETFILTER)?.interception(
            &input,
            destination,
            relay_port,
            wire::Protocol::Tcp,
            &deadline,
        )
    })?;
    let mut out = io::stdout().lock();
    writeln!(out, "managed-intercept {{\"type\":\"{:?}\"}}", Phase::Installed)?;
    out.flush()?;
    drop(out);
    if input.relay == RelayMode::Absent {
        return relay_absent(
            relay_listener,
            &mut backend_worker,
            origin,
            destination,
            &deadline,
        );
    }
    let mut relay_worker = Worker::start(relay_listener, Reply::Relay, &deadline)?;
    let result = client(origin, destination, Reply::Relay, &deadline).result;
    // Join both workers before propagating even a failed/mutated client result.
    let relay_result = relay_worker.finish();
    let backend_result = backend_worker.finish();
    let relay = relay_result?;
    let backend = backend_result?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"{:?}\",\"backend_accepted\":{},\"relay_accepted\":{},\"backend_receipts\":{},\"relay_receipts\":{},\"backend_terminal\":\"{:?}\",\"relay_terminal\":\"{:?}\",\"joined\":true}}",
        Phase::Joined,
        backend.report.accepted,
        relay.report.accepted,
        backend.report.receipts,
        relay.report.receipts,
        backend.outcome(),
        relay.outcome()
    )?;
    out.flush()?;
    drop(out);
    relay.record_error(Reply::Relay)?;
    backend.record_error(Reply::Backend)?;
    relay.result?;
    backend.result?;
    let relay_report = relay.report;
    let backend_report = backend.report;
    let client_tuple = result?;
    let accepted = relay_report
        .last
        .ok_or_else(|| invalid("relay accepted no connection"))?;
    if accepted.local != destination
        || accepted.peer != client_tuple.local
        || client_tuple.peer != destination
        || relay_report.accepted != 1
        || backend_report.accepted != 1
        || relay_report.receipts != 1
        || backend_report.receipts != 1
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
            assert!(Inputs::parse(Family::Ipv4, "local", "1000", mark, table, priority).is_err());
        }
        assert!(Inputs::parse(Family::Ipv4, "local", "0", "1", "100", "100").is_err());
        assert!(Inputs::parse(Family::Ipv4, "ipv6", "1000", "1", "100", "100").is_err());
        assert_eq!(
            Inputs::parse(Family::Ipv4, "forward", "1000", "1", "1000", "100")
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
                Inputs::parse(Family::Ipv4, name, "1000", "1", "1000", "100")
                    .unwrap()
                    .origin,
                origin
            );
        }
        let absent = Inputs::parse(Family::Ipv4, "forward-relay-absent", "1000", "1", "1000", "100").unwrap();
        assert_eq!(absent.origin, Origin::Forward);
        assert_eq!(absent.relay, RelayMode::Absent);
        assert_eq!(
            Inputs::parse(Family::Ipv4, "forward", "1000", "1", "1000", "100")
                .unwrap()
                .relay,
            RelayMode::Present
        );
    }

    #[test]
    fn absence_never_counts_success_or_unknown_errors_as_refusal() {
        let at = SocketAddr::from((BACKEND, 43210));
        let baseline = || Completion {
            report: Report {
                accepted: 1,
                receipts: 1,
                last: Some(Tuple { local: at, peer: at }),
            },
            result: Ok(()),
        };
        let attempt = |kind| ClientCompletion {
            stage: ClientStage::ConnectCall,
            result: Err(io::Error::from(kind)),
        };
        let success = ClientCompletion {
            stage: ClientStage::ConnectedIo,
            result: Ok(Tuple { local: at, peer: at }),
        };
        assert_eq!(attempt_outcome(&success), AttemptOutcome::Success);
        assert_eq!(
            absence_verdict(&success, &baseline()),
            AbsenceVerdict::DirectBackendEffect
        );
        let refused = attempt(io::ErrorKind::ConnectionRefused);
        assert_eq!(attempt_outcome(&refused), AttemptOutcome::Refused);
        assert_eq!(
            absence_verdict(&refused, &baseline()),
            AbsenceVerdict::ContainedRefusal
        );
        let timeout = attempt(io::ErrorKind::TimedOut);
        assert_eq!(attempt_outcome(&timeout), AttemptOutcome::Deadline);
        assert_eq!(
            absence_verdict(&timeout, &baseline()),
            AbsenceVerdict::NoNewBackendEffectBeforeDeadline
        );
        let setup = ClientCompletion {
            stage: ClientStage::BeforeConnect,
            result: Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
        };
        assert_eq!(attempt_outcome(&setup), AttemptOutcome::Error);
        assert_eq!(absence_verdict(&setup, &baseline()), AbsenceVerdict::Inconclusive);
        for kind in [io::ErrorKind::ConnectionRefused, io::ErrorKind::TimedOut] {
            let connected = ClientCompletion {
                stage: ClientStage::ConnectedIo,
                result: Err(io::Error::from(kind)),
            };
            assert_eq!(attempt_outcome(&connected), AttemptOutcome::Error);
            assert_eq!(
                absence_verdict(&connected, &baseline()),
                AbsenceVerdict::Inconclusive
            );
        }
        let unknown = Completion {
            report: Report::default(),
            result: Ok(()),
        };
        assert_eq!(absence_verdict(&refused, &unknown), AbsenceVerdict::Inconclusive);
        let effects = Completion {
            report: Report {
                accepted: 2,
                receipts: 1,
                last: None,
            },
            result: Err(io::Error::from_raw_os_error(libc::ENOTCONN)),
        };
        assert_eq!(effects.outcome(), WorkerOutcome::Error);
        assert_eq!(
            absence_verdict(&timeout, &effects),
            AbsenceVerdict::DirectBackendEffect
        );
        assert_eq!(effects.result.unwrap_err().raw_os_error(), Some(libc::ENOTCONN));
    }

    #[test]
    fn deadline_verdict_requires_baseline_counts_and_known_terminal() {
        let at = SocketAddr::from((BACKEND, 43210));
        let client = |stage, kind| ClientCompletion {
            stage,
            result: Err(io::Error::from(kind)),
        };
        let deadline = client(ClientStage::ConnectCall, io::ErrorKind::TimedOut);
        let backend = |accepted, receipts, last, kind: Option<io::ErrorKind>| Completion {
            report: Report {
                accepted,
                receipts,
                last,
            },
            result: kind.map_or(Ok(()), |kind| Err(io::Error::from(kind))),
        };
        let tuple = Some(Tuple { local: at, peer: at });
        for kind in [None, Some(io::ErrorKind::TimedOut)] {
            let completed = backend(1, 1, tuple, kind);
            assert_eq!(
                absence_verdict(&deadline, &completed),
                AbsenceVerdict::NoNewBackendEffectBeforeDeadline
            );
        }
        for completed in [
            backend(0, 0, None, None),
            backend(1, 0, tuple, None),
            backend(0, 1, tuple, None),
            backend(1, 1, tuple, Some(io::ErrorKind::NotConnected)),
        ] {
            assert_eq!(
                absence_verdict(&deadline, &completed),
                AbsenceVerdict::Inconclusive
            );
        }
        for stage in [ClientStage::BeforeConnect, ClientStage::ConnectedIo] {
            assert_eq!(
                absence_verdict(
                    &client(stage, io::ErrorKind::TimedOut),
                    &backend(1, 1, tuple, None)
                ),
                AbsenceVerdict::Inconclusive
            );
        }
        for completed in [
            backend(2, 1, tuple, Some(io::ErrorKind::NotConnected)),
            backend(1, 2, tuple, Some(io::ErrorKind::TimedOut)),
        ] {
            assert_eq!(
                absence_verdict(&deadline, &completed),
                AbsenceVerdict::DirectBackendEffect
            );
        }
        let success = ClientCompletion {
            stage: ClientStage::ConnectedIo,
            result: Ok(Tuple { local: at, peer: at }),
        };
        assert_eq!(
            absence_verdict(&success, &backend(0, 0, None, Some(io::ErrorKind::NotConnected))),
            AbsenceVerdict::DirectBackendEffect
        );
    }

    /// Makes accept4(2) itself fail with ECONNABORTED on this thread and the threads it
    /// starts, as a connection aborted before its accept would: a seccomp filter
    /// (seccomp(2), SECCOMP_RET_ERRNO) on this thread alone, without TSYNC.
    fn accept_aborts_here() {
        // linux/audit.h: EM_X86_64 (62) or EM_AARCH64 (183) | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE.
        #[cfg(target_arch = "x86_64")]
        const ARCH: u32 = 62 | 0x8000_0000 | 0x4000_0000;
        #[cfg(target_arch = "aarch64")]
        const ARCH: u32 = 183 | 0x8000_0000 | 0x4000_0000;
        let code = |code: u32| u16::try_from(code).unwrap();
        let load = |offset| libc::sock_filter {
            code: code(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS),
            jt: 0,
            jf: 0,
            k: offset,
        };
        let equal = |k, jt, jf| libc::sock_filter {
            code: code(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K),
            jt,
            jf,
            k,
        };
        let answer = |k| libc::sock_filter {
            code: code(libc::BPF_RET | libc::BPF_K),
            jt: 0,
            jf: 0,
            k,
        };
        // struct seccomp_data: nr at offset 0, arch at offset 4.
        let filter = [
            load(4),
            equal(ARCH, 1, 0),
            answer(libc::SECCOMP_RET_ALLOW),
            load(0),
            equal(u32::try_from(libc::SYS_accept4).unwrap(), 0, 1),
            answer(libc::SECCOMP_RET_ERRNO | u32::try_from(libc::ECONNABORTED).unwrap()),
            answer(libc::SECCOMP_RET_ALLOW),
        ];
        let program = libc::sock_fprog {
            len: u16::try_from(filter.len()).unwrap(),
            filter: filter.as_ptr().cast_mut(),
        };
        // SAFETY: prctl(2) with integer arguments, for this thread.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }, 0);
        assert_eq!(
            // SAFETY: a filter program the kernel copies before this returns.
            unsafe {
                libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &raw const program,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
    }

    /// The latest deadline an Instant holds. These workers end at an answer or a wake that
    /// Deadline::io's poll sees, never at time, so the tests choose no time limit of their own.
    fn farthest() -> Deadline {
        let mut within = Duration::MAX;
        loop {
            if let Ok(deadline) = Deadline::new(within) {
                return deadline;
            }
            within /= 2;
        }
    }

    /// Whether a thread of this process is in accept4(2) on `fd` (proc(5)'s
    /// /proc/pid/task/tid/syscall: the number, then the arguments in hex).
    fn blocked_in_accept4(fd: RawFd) -> bool {
        let wanted = format!("{} {fd:#x} ", libc::SYS_accept4);
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(Result::ok)
            .any(|task| {
                std::fs::read_to_string(task.path().join("syscall"))
                    .is_ok_and(|line| line.starts_with(&wanted))
            })
    }

    /// The hang CI's aarch64-unknown-linux-gnu job met (run 43496145972): Deadline::io tries
    /// its operation before it polls, so an accept4 on a blocking listener is in the kernel
    /// where neither the wake nor the deadline reaches it, and only a connection ends it.
    #[test]
    fn a_wake_never_reaches_an_accept_that_blocks() {
        // The wait below ends only where the kernel reports its threads' system calls.
        std::fs::read_to_string("/proc/thread-self/syscall").unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let at = listener.local_addr().unwrap();
        let fd = listener.as_raw_fd();
        let mut worker = Worker::spawn(&farthest(), move |deadline| {
            deadline
                .io(fd, libc::POLLIN, || listener.accept())
                .map(|(_, peer)| peer)
        })
        .unwrap();
        // A blocking accept4 stays in the kernel until a connection comes: none comes yet.
        while !blocked_in_accept4(fd) {
            if let WorkerState::Running(thread) = &worker.state {
                assert!(!thread.is_finished(), "the worker ended before its accept4");
            }
            std::thread::yield_now();
        }
        worker.wake();
        assert!(blocked_in_accept4(fd), "the wake moved a blocking accept4");
        let client = TcpStream::connect(at).unwrap();
        // The accept completed after its wake: the wake never reached it.
        let peer = worker.join().unwrap().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
    }

    #[test]
    fn an_aborted_accept_without_cancellation_is_a_worker_error() {
        let deadline = farthest();
        let aborted = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    accept_aborts_here();
                    let listener =
                        listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), SocketMode::Native).unwrap();
                    // The filter's own answer, before the worker relies on it.
                    let probe = listener.accept().map(|_| ()).unwrap_err();
                    assert_eq!(
                        probe.raw_os_error(),
                        Some(libc::ECONNABORTED),
                        "accept4 answered {probe}: the seccomp filter is not in force"
                    );
                    let mut worker = Worker::start(listener, Reply::Backend, &deadline).unwrap();
                    // Joined without its wake: nothing requested this worker's cancellation.
                    worker.join().unwrap()
                })
                .join()
                .unwrap()
        });
        assert_eq!(aborted.outcome(), WorkerOutcome::Error, "{:?}", aborted.result);
        assert_eq!(
            aborted.result.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        // A requested cancellation remains the worker's clean stop, on a listener given blocking.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let at = listener.local_addr().unwrap();
        // try_clone shares the open file description, and with it O_NONBLOCK.
        let shared = listener.try_clone().unwrap();
        let mut worker = Worker::start(listener, Reply::Backend, &deadline).unwrap();
        // SAFETY: F_GETFL on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(shared.as_raw_fd(), libc::F_GETFL) };
        let nonblocking = flags >= 0 && flags & libc::O_NONBLOCK != 0;
        if !nonblocking {
            // Releases a blocking accept4, so the failed test's Drop can join the worker.
            let _ = TcpStream::connect(at);
        }
        assert!(
            nonblocking,
            "the worker's listener is blocking: its accept4 never sees the wake"
        );
        let stopped = worker.finish().unwrap();
        assert_eq!(stopped.outcome(), WorkerOutcome::Stopped, "{:?}", stopped.result);
    }

    #[test]
    fn ipv6_family_uses_the_same_finite_origins_and_input_authority() {
        for name in [
            "local",
            "forward",
            "local-loopback",
            "local-self",
            "forward-relay-absent",
        ] {
            let input = Inputs::parse(Family::Ipv6, name, "1000", "1", "1000", "100").unwrap();
            assert_eq!(input.family, Family::Ipv6);
            assert_eq!(
                input.relay,
                if name == "forward-relay-absent" {
                    RelayMode::Absent
                } else {
                    RelayMode::Present
                }
            );
        }
        assert!(Inputs::parse(Family::Ipv6, "local", "0", "1", "1000", "100").is_err());
        assert!(Inputs::parse(Family::Ipv6, "local", "1000", "0", "1000", "100").is_err());
        assert!(Inputs::parse(Family::Ipv6, "local", "1000", "1", "254", "100").is_err());
    }
}
