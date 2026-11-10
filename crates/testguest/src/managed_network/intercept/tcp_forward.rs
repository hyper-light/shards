//! TCP fixture connect/tuple/orchestration; the shared init module owns the only pump.

use super::shared::{
    self, Attempt, ByteLimit, Control, EndPolicy, Fault, Half, Leg, Operation, Progress, Slots, SourceEnd,
    Termination, control, count, native,
};
use super::*;
use std::os::fd::AsFd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Connect {
    NotAttempted,
    Attempted,
    Pending,
    Connected,
}

#[derive(Debug)]
pub(super) struct Completion {
    pub(super) accepted: usize,
    pub(super) downstream: Option<Tuple>,
    pub(super) upstream: Option<Tuple>,
    pub(super) connect: Connect,
    pub(super) flow: shared::Report,
    pub(super) result: Result<Termination, Fault>,
}

fn connect(
    upstream: &TcpStream,
    destination: SocketAddr,
    deadline: &Deadline,
    state: &mut Connect,
) -> Result<(), Fault> {
    let admission = Control::Until(deadline);
    let SocketAddr::V4(at) = destination else {
        return Err(Fault::Bounds);
    };
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as _,
        sin_port: at.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(at.ip().octets()),
        },
        sin_zero: [0; 8],
    };
    let result = deadline
        .io(upstream.as_raw_fd(), libc::POLLOUT, || {
            *state = Connect::Attempted;
            // SAFETY: exact initialized IPv4 sockaddr; socket was created in the held router namespace.
            let n = unsafe {
                libc::connect(
                    upstream.as_raw_fd(),
                    (&raw const address).cast(),
                    size_of_val(&address) as _,
                )
            };
            // Even EINTR is a retained connect result, never a replayed connect.
            Ok(count(n as isize))
        })
        .map_err(control)?;
    match result {
        Ok(_) => {}
        Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {
            *state = Connect::Pending;
            shared::wait(&admission, -1, 0, upstream.as_raw_fd(), libc::POLLOUT)?;
            match shared::attempt(
                &admission,
                upstream.as_raw_fd(),
                libc::POLLOUT,
                Leg::Backend,
                Operation::Connect,
                || upstream.take_error(),
            )? {
                Attempt::Ready(None) => {}
                Attempt::Ready(Some(error)) => return Err(native(Leg::Backend, Operation::Connect, error)),
                Attempt::WouldBlock => return Err(Fault::Control(io::ErrorKind::WouldBlock)),
            }
        }
        Err(error) => return Err(native(Leg::Backend, Operation::Connect, error)),
    }
    if upstream
        .peer_addr()
        .map_err(|e| native(Leg::Backend, Operation::Connect, e))?
        != destination
    {
        return Err(Fault::Bounds);
    }
    *state = Connect::Connected;
    Ok(())
}

pub(super) fn forward(
    listener: TcpListener,
    upstream: TcpStream,
    destination: SocketAddr,
    slots: Slots,
    limits: [usize; 2],
    deadline: Deadline,
) -> Completion {
    let mut report = Completion {
        accepted: 0,
        downstream: None,
        upstream: None,
        connect: Connect::NotAttempted,
        flow: shared::Report::new(),
        result: Ok(Termination::BothHalvesClosed),
    };
    let admission = Control::Until(&deadline);
    report.result = (|| {
        if deadline.cancel.is_none() {
            return Err(Fault::Control(io::ErrorKind::InvalidInput));
        }
        let downstream = loop {
            match shared::attempt(
                &admission,
                listener.as_raw_fd(),
                libc::POLLIN,
                Leg::Client,
                Operation::Read,
                || listener.accept(),
            )? {
                Attempt::Ready((stream, _)) => {
                    report.accepted = 1;
                    break stream;
                }
                Attempt::WouldBlock => shared::wait(&admission, listener.as_raw_fd(), libc::POLLIN, -1, 0)?,
            }
        };
        run_accepted(
            downstream,
            upstream,
            destination,
            slots,
            limits,
            &deadline,
            &mut report,
        )
    })();
    report
}

pub(super) fn start(
    listener: TcpListener,
    upstream: TcpStream,
    destination: SocketAddr,
    slots: Slots,
    limits: [usize; 2],
    deadline: &Deadline,
) -> io::Result<Worker<Completion>> {
    Worker::spawn(deadline, move |deadline| {
        forward(listener, upstream, destination, slots, limits, deadline)
    })
}

fn run_accepted(
    downstream: TcpStream,
    upstream: TcpStream,
    destination: SocketAddr,
    slots: Slots,
    limits: [usize; 2],
    deadline: &Deadline,
    report: &mut Completion,
) -> Result<Termination, Fault> {
    downstream
        .set_nonblocking(true)
        .map_err(|e| native(Leg::Client, Operation::Read, e))?;
    report.downstream = Some(Tuple {
        local: downstream
            .local_addr()
            .map_err(|e| native(Leg::Client, Operation::Read, e))?,
        peer: downstream
            .peer_addr()
            .map_err(|e| native(Leg::Client, Operation::Read, e))?,
    });
    upstream
        .set_nonblocking(true)
        .map_err(|e| native(Leg::Backend, Operation::Connect, e))?;
    connect(&upstream, destination, deadline, &mut report.connect)?;
    report.upstream = Some(Tuple {
        local: upstream
            .local_addr()
            .map_err(|e| native(Leg::Backend, Operation::Connect, e))?,
        peer: upstream
            .peer_addr()
            .map_err(|e| native(Leg::Backend, Operation::Connect, e))?,
    });
    shared::pump(
        [downstream.as_fd(), upstream.as_fd()],
        slots,
        limits.map(ByteLimit::Bytes),
        Control::Until(deadline),
        EndPolicy::Independent,
        &mut report.flow,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    ServerFirst,
    BackendFinFirst,
}

#[derive(Debug)]
pub(in crate::managed_network) struct Inputs {
    network: super::Inputs,
    scenario: Scenario,
    slot_sizes: [usize; 2],
    request: Vec<u8>,
    greeting: Vec<u8>,
    tail: Vec<u8>,
}

impl Inputs {
    pub(in crate::managed_network) fn parse(words: &[&str]) -> io::Result<Self> {
        let [
            "intercept-tcp-forward4",
            scenario,
            millis,
            mark,
            table,
            priority,
            up,
            down,
            request,
            greeting,
            tail,
        ] = words
        else {
            return Err(invalid(
                "TCP forward expects scenario/time/routing/two slots/three hex payloads",
            ));
        };
        let size = |s: &str| {
            s.parse::<usize>()
                .map_err(|_| invalid("TCP slot size is an unsigned integer"))
        };
        let result = Self {
            network: super::Inputs::parse(Family::Ipv4, "forward", millis, mark, table, priority)?,
            scenario: match *scenario {
                "server-first" => Scenario::ServerFirst,
                "backend-fin-first" => Scenario::BackendFinFirst,
                _ => return Err(invalid("unsupported TCP forward scenario")),
            },
            slot_sizes: [size(up)?, size(down)?],
            request: payload(request)?,
            greeting: payload(greeting)?,
            tail: payload(tail)?,
        };
        if result.request.is_empty() || result.greeting.is_empty() {
            return Err(invalid(
                "TCP two-direction witness needs a nonempty request and greeting",
            ));
        }
        result
            .greeting
            .len()
            .checked_add(result.tail.len())
            .ok_or_else(|| invalid("TCP response overflow"))?;
        Ok(result)
    }

    fn duplicate(&self) -> io::Result<Self> {
        fn copy(bytes: &[u8]) -> io::Result<Vec<u8>> {
            let mut result = Vec::new();
            result.try_reserve_exact(bytes.len()).map_err(io::Error::other)?;
            result.extend_from_slice(bytes);
            Ok(result)
        }
        Ok(Self {
            network: super::Inputs {
                family: self.network.family,
                origin: self.network.origin,
                relay: self.network.relay,
                within: self.network.within,
                mark: self.network.mark,
                table: self.network.table,
                priority: self.network.priority,
            },
            scenario: self.scenario,
            slot_sizes: self.slot_sizes,
            request: copy(&self.request)?,
            greeting: copy(&self.greeting)?,
            tail: copy(&self.tail)?,
        })
    }
}

fn payload(word: &str) -> io::Result<Vec<u8>> {
    let Some(repeated) = word.strip_prefix("repeat:") else {
        return unhex(word);
    };
    let (size, seed) = repeated
        .split_once(':')
        .ok_or_else(|| invalid("repeated TCP payload expects byte-count:hex-seed"))?;
    let size = size
        .parse::<usize>()
        .map_err(|_| invalid("TCP payload byte count is unsigned"))?;
    let seed = unhex(seed)?;
    if seed.is_empty() {
        return Err(invalid("repeated TCP payload seed is empty"));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(io::Error::other)?;
    while bytes.len() < size {
        let left = size
            .checked_sub(bytes.len())
            .ok_or_else(|| invalid("TCP payload count overflow"))?;
        let part = seed
            .get(..left.min(seed.len()))
            .ok_or_else(|| invalid("TCP payload seed envelope"))?;
        bytes.extend_from_slice(part);
    }
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Open,
    Closed,
}

struct Endpoint {
    bytes: Vec<u8>,
    read: usize,
    written: usize,
    read_end: End,
    write_end: End,
    extra: Option<u8>,
}

impl Endpoint {
    fn reserve(size: usize) -> io::Result<Self> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(io::Error::other)?;
        bytes.resize(size, 0);
        Ok(Self {
            bytes,
            read: 0,
            written: 0,
            read_end: End::Open,
            write_end: End::Open,
            extra: None,
        })
    }

    fn reset(&mut self) {
        self.read = 0;
        self.written = 0;
        self.read_end = End::Open;
        self.write_end = End::Open;
        self.extra = None;
    }

    fn send(&mut self, stream: &mut TcpStream, mut bytes: &[u8], deadline: &Deadline) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = deadline.io(stream.as_raw_fd(), libc::POLLOUT, || stream.write(bytes))?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.written = self
                .written
                .checked_add(n)
                .ok_or_else(|| invalid("endpoint send count overflow"))?;
            bytes = bytes
                .get(n..)
                .ok_or_else(|| invalid("endpoint send count exceeds buffer"))?;
        }
        Ok(())
    }

    fn receive(&mut self, stream: &mut TcpStream, expected: &[u8], deadline: &Deadline) -> io::Result<()> {
        let end = self
            .read
            .checked_add(expected.len())
            .ok_or_else(|| invalid("endpoint receive overflow"))?;
        let start = self.read;
        while self.read < end {
            let bytes = self
                .bytes
                .get_mut(self.read..end)
                .ok_or_else(|| invalid("endpoint receive envelope"))?;
            let n = deadline.io(stream.as_raw_fd(), libc::POLLIN, || stream.read(bytes))?;
            if n == 0 {
                self.read_end = End::Closed;
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if n > bytes.len() {
                return Err(invalid("endpoint receive count exceeds buffer"));
            }
            self.read = self
                .read
                .checked_add(n)
                .ok_or_else(|| invalid("endpoint receive count overflow"))?;
        }
        if self.bytes.get(start..end) != Some(expected) {
            return Err(invalid("actual TCP endpoint bytes differ"));
        }
        Ok(())
    }

    fn eof(&mut self, stream: &mut TcpStream, deadline: &Deadline) -> io::Result<()> {
        let mut extra = [0u8; 1];
        let n = deadline.io(stream.as_raw_fd(), libc::POLLIN, || stream.read(&mut extra))?;
        if n != 0 {
            self.extra = extra.first().copied();
            return Err(invalid("unexpected extra TCP endpoint byte"));
        }
        self.read_end = End::Closed;
        Ok(())
    }

    fn close_write(&mut self, stream: &TcpStream) -> io::Result<()> {
        stream.shutdown(Shutdown::Write)?;
        self.write_end = End::Closed;
        Ok(())
    }

    fn record(&self, role: &str, ordinal: usize, tuple: Tuple, result: &io::Result<()>) -> io::Result<()> {
        let mut out = io::stdout().lock();
        write!(
            out,
            "managed-tcp-forward {{\"type\":\"Endpoint\",\"role\":\"{role}\",\"ordinal\":{ordinal},\"local\":\"{}\",\"peer\":\"{}\",\"read\":{},\"written\":{},\"read_end\":\"{:?}\",\"write_end\":\"{:?}\",\"ok\":{},\"read_hex\":\"",
            tuple.local,
            tuple.peer,
            self.read,
            self.written,
            self.read_end,
            self.write_end,
            result.is_ok()
        )?;
        for byte in self
            .bytes
            .get(..self.read)
            .ok_or_else(|| invalid("endpoint retained count"))?
        {
            write!(out, "{byte:02x}")?;
        }
        write!(out, "\",\"extra_hex\":")?;
        match self.extra {
            Some(byte) => write!(out, "\"{byte:02x}\"")?,
            None => write!(out, "null")?,
        }
        write!(
            out,
            ",\"error_kind\":\"{:?}\",\"errno\":",
            result.as_ref().err().map(io::Error::kind)
        )?;
        match result.as_ref().err().and_then(|error| error.raw_os_error()) {
            Some(errno) => write!(out, "{errno}")?,
            None => write!(out, "null")?,
        }
        writeln!(out, "}}")?;
        out.flush()
    }
}

fn server_script(
    stream: &mut TcpStream,
    input: &Inputs,
    endpoint: &mut Endpoint,
    deadline: &Deadline,
) -> io::Result<()> {
    endpoint.send(stream, &input.greeting, deadline)?;
    if input.scenario == Scenario::BackendFinFirst {
        endpoint.send(stream, &input.tail, deadline)?;
        endpoint.close_write(stream)?;
    }
    endpoint.receive(stream, &input.request, deadline)?;
    endpoint.eof(stream, deadline)?;
    if input.scenario == Scenario::ServerFirst {
        endpoint.send(stream, &input.tail, deadline)?;
        endpoint.close_write(stream)?;
    }
    Ok(())
}

struct BackendDone {
    accepted: usize,
    last: Option<Tuple>,
    result: io::Result<()>,
}

fn backend_script(
    listener: TcpListener,
    input: Inputs,
    mut endpoint: Endpoint,
    deadline: Deadline,
) -> BackendDone {
    let mut done = BackendDone {
        accepted: 0,
        last: None,
        result: Ok(()),
    };
    done.result = (|| {
        // Exactly baseline and mediated conversations, not an open-ended server.
        for ordinal in [0, 1] {
            let (mut stream, peer) = deadline.io(listener.as_raw_fd(), libc::POLLIN, || listener.accept())?;
            done.accepted = done
                .accepted
                .checked_add(1)
                .ok_or_else(|| invalid("backend accept overflow"))?;
            stream.set_nonblocking(true)?;
            let tuple = Tuple {
                local: stream.local_addr()?,
                peer,
            };
            done.last = Some(tuple);
            endpoint.reset();
            let result = server_script(&mut stream, &input, &mut endpoint, &deadline);
            endpoint.record("Backend", ordinal, tuple, &result)?;
            result?;
        }
        Ok(())
    })();
    done
}

fn client_script(
    namespace: &Namespace,
    destination: SocketAddr,
    input: &Inputs,
    endpoint: &mut Endpoint,
    ordinal: usize,
    deadline: &Deadline,
) -> io::Result<(TcpStream, Tuple)> {
    in_namespace(Some(&namespace.handle), || {
        let mut stream = TcpStream::connect_timeout(&destination, deadline.left()?)?;
        stream.set_nonblocking(true)?;
        let tuple = Tuple {
            local: stream.local_addr()?,
            peer: stream.peer_addr()?,
        };
        endpoint.reset();
        let result = client_exchange(&mut stream, input, endpoint, deadline);
        endpoint.record("Client", ordinal, tuple, &result)?;
        result?;
        Ok((stream, tuple))
    })
}

fn client_exchange(
    stream: &mut TcpStream,
    input: &Inputs,
    endpoint: &mut Endpoint,
    deadline: &Deadline,
) -> io::Result<()> {
    endpoint.receive(stream, &input.greeting, deadline)?;
    if input.scenario == Scenario::BackendFinFirst {
        endpoint.receive(stream, &input.tail, deadline)?;
        endpoint.eof(stream, deadline)?;
    }
    endpoint.send(stream, &input.request, deadline)?;
    endpoint.close_write(stream)?;
    if input.scenario == Scenario::ServerFirst {
        endpoint.receive(stream, &input.tail, deadline)?;
        endpoint.eof(stream, deadline)?;
    }
    Ok(())
}

fn progress_record(out: &mut impl Write, progress: &Progress) -> io::Result<()> {
    let (half, source) = match progress.half {
        Half::Reading => ("Reading", None),
        Half::Draining(cause) => ("Draining", Some(cause)),
        Half::WriteClosed(cause) => ("WriteClosed", Some(cause)),
    };
    write!(out, "{{\"half\":\"{half}\",\"source_end\":")?;
    match source {
        None => write!(out, "null")?,
        Some(SourceEnd::Eof) => write!(out, "\"Eof\"")?,
        Some(SourceEnd::PeerEnded) => write!(out, "\"PeerEnded\"")?,
    }
    write!(
        out,
        ",\"read\":{},\"written\":{},\"pending\":{},\"extra_consumed\":{},\"partial_writes\":{},\"would_block\":{}}}",
        progress.read,
        progress.written,
        progress.pending,
        progress.extra_consumed,
        progress.partial_writes,
        progress.would_block
    )
}

fn fault_record<T>(out: &mut impl Write, result: &Result<T, Fault>) -> io::Result<()> {
    match result {
        Ok(_) => write!(out, "null"),
        Err(Fault::Cancelled) => write!(out, "{{\"kind\":\"Cancelled\"}}"),
        Err(Fault::Expired) => write!(out, "{{\"kind\":\"Expired\"}}"),
        Err(Fault::Allocation) => write!(out, "{{\"kind\":\"Allocation\"}}"),
        Err(Fault::Reservation { kind, errno }) => {
            write!(
                out,
                "{{\"kind\":\"Reservation\",\"error_kind\":\"{kind:?}\",\"errno\":"
            )?;
            match errno {
                Some(errno) => write!(out, "{errno}")?,
                None => write!(out, "null")?,
            }
            write!(out, "}}")
        }
        Err(Fault::Bounds) => write!(out, "{{\"kind\":\"Bounds\"}}"),
        Err(Fault::Control(kind)) => write!(out, "{{\"kind\":\"Control\",\"error_kind\":\"{kind:?}\"}}"),
        Err(Fault::Limit { supplied, required }) => write!(
            out,
            "{{\"kind\":\"Limit\",\"supplied\":{supplied},\"required\":{required}}}"
        ),
        Err(Fault::ExtraData { leg, offset }) => write!(
            out,
            "{{\"kind\":\"ExtraData\",\"leg\":\"{leg:?}\",\"offset\":{offset}}}"
        ),
        Err(Fault::Native {
            leg,
            operation,
            kind,
            errno,
        }) => {
            write!(
                out,
                "{{\"kind\":\"Native\",\"leg\":\"{leg:?}\",\"operation\":\"{operation:?}\",\"error_kind\":\"{kind:?}\",\"errno\":"
            )?;
            match errno {
                Some(errno) => write!(out, "{errno}")?,
                None => write!(out, "null")?,
            }
            write!(out, "}}")
        }
    }
}

pub(in crate::managed_network) fn run(input: Inputs) -> io::Result<()> {
    if std::process::id() != 1 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let before = descriptor_count()?;
    let result = run_owned(input);
    let after = descriptor_count()?;
    {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "managed-tcp-forward {{\"type\":\"Cleanup\",\"fd_before\":{before},\"fd_after\":{after},\"scope\":\"OwnedGuestDescriptors\",\"kernel_return\":\"Unknown\"}}"
        )?;
        out.flush()?;
    }
    result?;
    if before != after {
        return Err(invalid("owned TCP descriptor count did not return"));
    }
    Ok(())
}

fn descriptor_count() -> io::Result<usize> {
    std::fs::read_dir("/proc/self/fd")?.try_fold(0usize, |count, entry| {
        entry?;
        count
            .checked_add(1)
            .ok_or_else(|| invalid("descriptor count overflow"))
    })
}

fn run_owned(input: Inputs) -> io::Result<()> {
    let deadline = Deadline::new(input.network.within)?;
    let [up_size, down_size] = input.slot_sizes;
    let slots = match Slots::reserve(up_size, down_size) {
        Ok(slots) => slots,
        Err(fault) => {
            let kind = match &fault {
                Fault::Allocation => io::ErrorKind::OutOfMemory,
                _ => io::ErrorKind::InvalidInput,
            };
            let mut out = io::stdout().lock();
            write!(
                out,
                "managed-tcp-forward {{\"type\":\"AdmissionRefused\",\"fault\":"
            )?;
            fault_record(&mut out, &Err::<(), Fault>(fault))?;
            writeln!(out, "}}")?;
            out.flush()?;
            return Err(kind.into());
        }
    };
    let response = input
        .greeting
        .len()
        .checked_add(input.tail.len())
        .ok_or_else(|| invalid("response count overflow"))?;
    let limits = [input.request.len(), response];
    let backend_input = input.duplicate()?;
    let backend_endpoint = Endpoint::reserve(input.request.len())?;
    let mut client_endpoint = Endpoint::reserve(response)?;
    let [router, backend, client_ns] = prepare(&input.network, &deadline)?;
    let backend_inode = backend.inode()?;
    let backend_listener = in_namespace(Some(&backend.handle), || {
        listen(SocketAddr::from((BACKEND, 0)), SocketMode::Native)
    })?;
    let destination = backend_listener.local_addr()?;
    let relay_listener = in_namespace(Some(&router.handle), || {
        listen(
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            SocketMode::Transparent,
        )
    })?;
    let relay_port = relay_listener.local_addr()?.port();
    // A listener's namespace does not place a newly created upstream socket.
    let upstream = TcpStream::from(in_namespace(Some(&router.handle), || {
        bound(
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            libc::SOCK_STREAM,
            SocketMode::Native,
        )
    })?);
    let mut backend_worker = Worker::spawn(&deadline, move |deadline| {
        backend_script(backend_listener, backend_input, backend_endpoint, deadline)
    })?;
    let baseline = client_script(
        &client_ns,
        destination,
        &input,
        &mut client_endpoint,
        0,
        &deadline,
    )?;
    in_namespace(Some(&router.handle), || {
        router
            .route
            .policy(interface(c"lo")?, &input.network, destination, &deadline)?;
        Netlink::open(libc::NETLINK_NETFILTER)?.interception(
            &input.network,
            destination,
            relay_port,
            wire::Protocol::Tcp,
            &deadline,
        )
    })?;
    let mut relay_worker = start(relay_listener, upstream, destination, slots, limits, &deadline)?;
    let client = client_script(
        &client_ns,
        destination,
        &input,
        &mut client_endpoint,
        1,
        &deadline,
    );
    let relay = if client.is_ok() {
        relay_worker.join()
    } else {
        relay_worker.finish()
    };
    let backend = match &relay {
        Ok(done) if done.result.is_ok() => backend_worker.join(),
        _ => backend_worker.finish(),
    };
    {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "managed-tcp-forward {{\"type\":\"Joined\",\"relay_joined\":{},\"backend_joined\":{}}}",
            relay.is_ok(),
            backend.is_ok()
        )?;
        out.flush()?;
    }
    let relay = relay?;
    let backend = backend?;
    {
        let mut out = io::stdout().lock();
        write!(
            out,
            "managed-tcp-forward {{\"type\":\"Completion\",\"connect\":\"{:?}\",\"accepted\":{},\"backend_accepted\":{},\"ok\":{},\"fault\":",
            relay.connect,
            relay.accepted,
            backend.accepted,
            relay.result.is_ok()
        )?;
        fault_record(&mut out, &relay.result)?;
        write!(out, ",\"to_backend\":")?;
        progress_record(&mut out, &relay.flow.to_backend)?;
        write!(out, ",\"to_client\":")?;
        progress_record(&mut out, &relay.flow.to_client)?;
        writeln!(out, "}}")?;
        out.flush()?;
    }
    backend.result?;
    let client = client?;
    if relay.result.is_err() {
        return Err(io::ErrorKind::Other.into());
    }
    let accepted = relay.downstream.ok_or_else(|| invalid("no downstream tuple"))?;
    let upstream = relay.upstream.ok_or_else(|| invalid("no upstream tuple"))?;
    if relay.accepted != 1
        || backend.accepted != 2
        || accepted.local != destination
        || accepted.peer != client.1.local
        || client.1.peer != destination
        || upstream.peer != destination
        || upstream.local == client.1.local
        || backend.last
            != Some(Tuple {
                local: destination,
                peer: upstream.local,
            })
        || relay.flow.to_backend.read != input.request.len()
        || relay.flow.to_backend.written != input.request.len()
        || relay.flow.to_client.read != response
        || relay.flow.to_client.written != response
        || relay.flow.to_backend.pending != 0
        || relay.flow.to_client.pending != 0
        || relay.flow.to_backend.half != Half::WriteClosed(SourceEnd::Eof)
        || relay.flow.to_client.half != Half::WriteClosed(SourceEnd::Eof)
    {
        return Err(invalid("TCP two-leg byte/half-close/tuple witness differs"));
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-tcp-forward {{\"type\":\"Exact\",\"destination\":\"{destination}\",\"client\":\"{}\",\"upstream\":\"{}\",\"baseline\":\"{}\",\"client_inode\":{},\"backend_inode\":{backend_inode},\"relay_inode\":{}}}",
        client.1.local,
        upstream.local,
        baseline.1.local,
        client_ns.inode()?,
        router.inode()?
    )?;
    out.flush()
}

#[cfg(test)]
mod native_tests {
    use super::*;

    #[test]
    #[ignore = "private native socket campaign supplies release FD and finite byte/time inputs"]
    fn native_tcp_pump_both_half_closes() {
        let supplied = |name| std::env::var(name).expect("explicit native fixture input");
        let words = [
            "intercept-tcp-forward4".to_owned(),
            supplied("SHARDS_TCP_SCENARIO"),
            supplied("SHARDS_NETWORK_CASE_MS"),
            supplied("SHARDS_INTERCEPT_MARK"),
            supplied("SHARDS_INTERCEPT_TABLE"),
            supplied("SHARDS_INTERCEPT_RULE_PRIORITY"),
            supplied("SHARDS_TCP_UP_BYTES"),
            supplied("SHARDS_TCP_DOWN_BYTES"),
            supplied("SHARDS_TCP_REQUEST_HEX"),
            supplied("SHARDS_TCP_GREETING_HEX"),
            supplied("SHARDS_TCP_TAIL_HEX"),
        ];
        let words: Vec<_> = words.iter().map(String::as_str).collect();
        let input = Inputs::parse(&words).unwrap();
        let deadline = Deadline::new(input.network.within).unwrap();
        let fd = supplied("SHARDS_NATIVE_RELEASE_FD").parse::<i32>().unwrap();
        assert!(fd > 2, "only a dedicated inherited control descriptor");
        // SAFETY: the private launcher transfers sole ownership of this FD.
        let release = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: query flags of this owned inherited descriptor.
        let flags = unsafe { libc::fcntl(release.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0 && flags & libc::O_NONBLOCK != 0);
        let mut word = [0u8; 1];
        // Trusted test-only release uses the SAME original absolute at, never
        // the cancellation-disabled control mode for actual forwarding I/O.
        let gate = Deadline {
            at: deadline.at,
            cancel: None,
        };
        let n = gate
            .io(release.as_raw_fd(), libc::POLLIN, || {
                // SAFETY: one initialized writable control byte, live owned pipe.
                count(unsafe { libc::read(release.as_raw_fd(), word.as_mut_ptr().cast(), word.len()) })
            })
            .unwrap();
        assert_eq!((n, word), (1, [b'R']));
        drop(release);
        let before = descriptor_count().unwrap();
        let [up_size, down_size] = input.slot_sizes;
        let storage = supplied("SHARDS_TCP_STORAGE");
        let slots = match storage.as_str() {
            "caller" => Slots::reserve(up_size, down_size).unwrap(),
            "minimum-mapped" => Slots::minimum_mapped().unwrap(),
            _ => panic!("explicit caller or minimum-mapped storage required"),
        };
        assert_eq!(slots.capacities(), input.slot_sizes);
        let response = input.greeting.len().checked_add(input.tail.len()).unwrap();
        let limits = [input.request.len(), response];
        let backend_input = input.duplicate().unwrap();
        let mut backend_endpoint = Endpoint::reserve(input.request.len()).unwrap();
        let mut client_endpoint = Endpoint::reserve(response).unwrap();
        let backend_listener =
            listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), SocketMode::Native).unwrap();
        let destination = backend_listener.local_addr().unwrap();
        let relay_listener = listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), SocketMode::Native).unwrap();
        let relay_at = relay_listener.local_addr().unwrap();
        let upstream = TcpStream::from(
            bound(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                libc::SOCK_STREAM,
                SocketMode::Native,
            )
            .unwrap(),
        );
        let mut backend_worker = Worker::spawn(&deadline, move |deadline| {
            let (mut stream, peer) = deadline
                .io(backend_listener.as_raw_fd(), libc::POLLIN, || {
                    backend_listener.accept()
                })
                .unwrap();
            stream.set_nonblocking(true).unwrap();
            let tuple = Tuple {
                local: stream.local_addr().unwrap(),
                peer,
            };
            let result = server_script(&mut stream, &backend_input, &mut backend_endpoint, &deadline);
            backend_endpoint
                .record("NativeBackend", 0, tuple, &result)
                .unwrap();
            (tuple, backend_endpoint, result)
        })
        .unwrap();
        let mut relay = start(relay_listener, upstream, destination, slots, limits, &deadline).unwrap();
        let mut client = TcpStream::connect_timeout(&relay_at, deadline.left().unwrap()).unwrap();
        client.set_nonblocking(true).unwrap();
        let client_tuple = Tuple {
            local: client.local_addr().unwrap(),
            peer: client.peer_addr().unwrap(),
        };
        let result = client_exchange(&mut client, &input, &mut client_endpoint, &deadline);
        client_endpoint
            .record("NativeClient", 0, client_tuple, &result)
            .unwrap();
        let forwarded = if result.is_ok() {
            relay.join()
        } else {
            relay.finish()
        }
        .unwrap();
        let backend = if forwarded.result.is_ok() {
            backend_worker.join()
        } else {
            backend_worker.finish()
        }
        .unwrap();
        result.unwrap();
        backend.2.unwrap();
        assert_eq!(*forwarded.result.as_ref().unwrap(), Termination::BothHalvesClosed);
        assert_eq!(forwarded.connect, Connect::Connected);
        assert_eq!(forwarded.accepted, 1);
        assert_eq!(
            forwarded.downstream,
            Some(Tuple {
                local: relay_at,
                peer: client_tuple.local
            })
        );
        assert_eq!(
            backend.0,
            Tuple {
                local: destination,
                peer: forwarded.upstream.unwrap().local
            }
        );
        {
            let mut out = io::stdout().lock();
            write!(out,"managed-tcp-forward {{\"type\":\"NativeFlow\",\"storage\":\"{storage}\",\"connect\":\"{:?}\",\"end\":\"{:?}\",\"client\":\"{}\",\"upstream\":\"{}\",\"to_backend\":",forwarded.connect,forwarded.result.as_ref().unwrap(),client_tuple.local,backend.0.peer).unwrap();
            progress_record(&mut out, &forwarded.flow.to_backend).unwrap();
            write!(out, ",\"to_client\":").unwrap();
            progress_record(&mut out, &forwarded.flow.to_client).unwrap();
            writeln!(out, "}}").unwrap();
            out.flush().unwrap();
        }
        assert_ne!(backend.0.peer, client_tuple.local);
        assert_eq!(forwarded.flow.to_backend.read, input.request.len());
        assert_eq!(forwarded.flow.to_backend.written, input.request.len());
        assert_eq!(forwarded.flow.to_client.read, response);
        assert_eq!(forwarded.flow.to_client.written, response);
        for progress in [&forwarded.flow.to_backend, &forwarded.flow.to_client] {
            assert_eq!(progress.half, Half::WriteClosed(SourceEnd::Eof));
            assert_eq!(progress.pending, 0);
            assert_eq!(progress.extra_consumed, 0);
        }
        drop(client);
        drop(relay);
        drop(backend_worker);
        let after = descriptor_count().unwrap();
        assert_eq!(after, before);
        println!(
            "NATIVE_TCP_EXACT scenario={:?} request={} response={} fd_before={before} fd_after={after} kernel_return=Unknown",
            input.scenario,
            input.request.len(),
            response
        );
    }
}
