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
pub(super) enum Abort {
    NotRequired,
    Requested {
        leg: Leg,
    },
    Failed {
        leg: Leg,
        kind: io::ErrorKind,
        errno: Option<i32>,
    },
}

#[derive(Debug)]
pub(super) struct Completion {
    pub(super) accepted: usize,
    pub(super) downstream: Option<Tuple>,
    pub(super) upstream: Option<Tuple>,
    pub(super) connect: Connect,
    pub(super) flow: shared::Report,
    pub(super) abort: Abort,
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
        abort: Abort::NotRequired,
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
    let result = shared::pump(
        [downstream.as_fd(), upstream.as_fd()],
        slots,
        limits.map(ByteLimit::Bytes),
        Control::Until(deadline),
        EndPolicy::Independent,
        &mut report.flow,
    );
    report.abort = reset_peer(&result, [&downstream, &upstream]);
    result
}

fn reset_peer(result: &Result<Termination, Fault>, streams: [&TcpStream; 2]) -> Abort {
    let Err(Fault::Native {
        leg,
        kind: io::ErrorKind::ConnectionReset,
        ..
    }) = result
    else {
        return Abort::NotRequired;
    };
    let [client, backend] = streams;
    let (socket, leg) = match leg {
        Leg::Client => (backend, Leg::Backend),
        Leg::Backend => (client, Leg::Client),
    };
    // Linux6.18.48 __tcp_close: zero SO_LINGER selects abortive close rather
    // than FIN. This is TCP-owner finalization after an actual peer reset;
    // setsockopt acceptance is a request, not proof its peer received RST.
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: this caller still owns both sockets, with an exact initialized
    // native option. They close on return; no new application I/O is admitted.
    if unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            size_of_val(&linger) as _,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        Abort::Failed {
            leg,
            kind: error.kind(),
            errno: error.raw_os_error(),
        }
    } else {
        Abort::Requested { leg }
    }
}

fn abort_record(out: &mut impl Write, abort: &Abort) -> io::Result<()> {
    match abort {
        Abort::NotRequired => write!(out, "{{\"kind\":\"NotRequired\"}}"),
        Abort::Requested { leg } => write!(out, "{{\"kind\":\"Requested\",\"leg\":\"{leg:?}\"}}"),
        Abort::Failed { leg, kind, errno } => {
            write!(
                out,
                "{{\"kind\":\"Failed\",\"leg\":\"{leg:?}\",\"error_kind\":\"{kind:?}\",\"errno\":"
            )?;
            match errno {
                Some(errno) => write!(out, "{errno}")?,
                None => write!(out, "null")?,
            }
            write!(out, "}}")
        }
    }
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

    fn send(
        &mut self,
        stream: &mut (impl Write + AsRawFd),
        mut bytes: &[u8],
        deadline: &Deadline,
    ) -> io::Result<()> {
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

    fn eof(&mut self, stream: &mut (impl Read + AsRawFd), deadline: &Deadline) -> io::Result<()> {
        let mut extra = [0u8; 1];
        let n = deadline.io(stream.as_raw_fd(), libc::POLLIN, || stream.read(&mut extra))?;
        if n != 0 {
            self.extra = extra.first().copied();
            return Err(invalid("unexpected extra TCP endpoint byte"));
        }
        self.read_end = End::Closed;
        Ok(())
    }

    fn close_write(&mut self, stream: &impl AsRawFd) -> io::Result<()> {
        // SAFETY: the caller retains this owned native stream for the call.
        count(unsafe { libc::shutdown(stream.as_raw_fd(), libc::SHUT_WR) } as isize)?;
        self.write_end = End::Closed;
        Ok(())
    }

    fn receive_to_end(&mut self, stream: &mut (impl Read + AsRawFd), deadline: &Deadline) -> io::Result<()> {
        loop {
            if self.read == self.bytes.len() {
                return self.eof(stream, deadline);
            }
            let bytes = self
                .bytes
                .get_mut(self.read..)
                .ok_or_else(|| invalid("endpoint receive envelope"))?;
            let n = deadline.io(stream.as_raw_fd(), libc::POLLIN, || stream.read(bytes))?;
            if n == 0 {
                self.read_end = End::Closed;
                return Ok(());
            }
            if n > bytes.len() {
                return Err(invalid("endpoint receive count exceeds buffer"));
            }
            self.read = self
                .read
                .checked_add(n)
                .ok_or_else(|| invalid("endpoint receive overflow"))?;
        }
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
        ",\"read\":{},\"written\":{},\"pending\":{},\"extra_consumed\":{},\"partial_writes\":{},\"would_block\":{},\"write_would_block\":{}}}",
        progress.read,
        progress.written,
        progress.pending,
        progress.extra_consumed,
        progress.partial_writes,
        progress.would_block,
        progress.write_would_block
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
        write!(out, ",\"abort\":")?;
        abort_record(&mut out, &relay.abort)?;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EngineHalf {
    ClientFinFirst,
    HostFinFirst,
}

#[derive(Debug)]
pub(in crate::managed_network) struct EngineInputs {
    half: EngineHalf,
    within: Duration,
    path: Vec<u8>,
    maximum: usize,
    request: Vec<u8>,
    suffix: Vec<u8>,
    late: Vec<u8>,
}

impl EngineInputs {
    pub(in crate::managed_network) fn parse(words: &[&str]) -> io::Result<Self> {
        let [
            "engine-halfclose",
            half,
            millis,
            path,
            maximum,
            request,
            suffix,
            late,
        ] = words
        else {
            return Err(invalid(
                "engine half-close expects mode/time/Unix-path/response-cap/request/suffix/late hex",
            ));
        };
        let mut name = Vec::new();
        name.try_reserve_exact(path.len()).map_err(io::Error::other)?;
        name.extend_from_slice(path.as_bytes());
        let input = Self {
            half: match *half {
                "client-fin-first" => EngineHalf::ClientFinFirst,
                "host-fin-first" => EngineHalf::HostFinFirst,
                _ => return Err(invalid("unknown engine half-close order")),
            },
            within: Duration::from_millis(
                millis
                    .parse()
                    .map_err(|_| invalid("engine deadline is unsigned"))?,
            ),
            path: name,
            maximum: maximum
                .parse()
                .map_err(|_| invalid("engine response cap is unsigned"))?,
            request: unhex(request)?,
            suffix: unhex(suffix)?,
            late: unhex(late)?,
        };
        if input.maximum == 0
            || input.request.is_empty()
            || input.suffix.is_empty()
            || input.path.is_empty()
            || input.path.contains(&0)
            || (input.half == EngineHalf::HostFinFirst && input.late.is_empty())
            || (input.half == EngineHalf::ClientFinFirst && !input.late.is_empty())
        {
            return Err(invalid(
                "engine half-close needs explicit nonempty finite witnesses",
            ));
        }
        Ok(input)
    }
}

#[derive(Clone, Copy, Debug)]
enum EnginePhase {
    Prepared,
    Connecting,
    Connected,
    RequestSent,
    ClientFin,
    ResponseEof,
}

#[derive(Debug)]
enum LateWrite {
    NotAttempted,
    Accepted(usize),
    Failed(io::ErrorKind, Option<i32>),
}

fn unix_connect(
    path: &[u8],
    deadline: &Deadline,
    phase: &mut EnginePhase,
) -> io::Result<std::os::unix::net::UnixStream> {
    // SAFETY: a fresh owned nonblocking native Unix socket.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful socket returned sole ownership of fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let stream = std::os::unix::net::UnixStream::from(fd);
    // SAFETY: all-zero sockaddr_un is a valid initialized address container.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    if path.len() >= address.sun_path.len() {
        return Err(invalid("engine Unix path exceeds native envelope"));
    }
    for (out, byte) in address.sun_path.iter_mut().zip(path) {
        *out = *byte as libc::c_char;
    }
    let size = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(path.len())
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| invalid("engine sockaddr overflow"))?;
    let size = libc::socklen_t::try_from(size).map_err(|_| invalid("engine sockaddr size overflow"))?;
    *phase = EnginePhase::Connecting;
    let result = deadline.io(stream.as_raw_fd(), libc::POLLOUT, || {
        // SAFETY: initialized pathname address, checked native size; one actual
        // connect attempt. Native EINTR/EAGAIN are retained, never replayed.
        Ok(count(
            unsafe { libc::connect(stream.as_raw_fd(), (&raw const address).cast(), size) } as isize,
        ))
    })?;
    match result {
        Ok(_) => {}
        Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {
            shared::wait(
                &Control::Until(deadline),
                -1,
                0,
                stream.as_raw_fd(),
                libc::POLLOUT,
            )
            .map_err(|fault| match fault {
                Fault::Expired => io::ErrorKind::TimedOut.into(),
                Fault::Cancelled => io::ErrorKind::ConnectionAborted.into(),
                Fault::Control(kind) => kind.into(),
                Fault::Native {
                    errno: Some(errno), ..
                } => io::Error::from_raw_os_error(errno),
                Fault::Native {
                    kind, errno: None, ..
                } => kind.into(),
                _ => io::ErrorKind::Other.into(),
            })?;
            if let Some(error) = stream.take_error()? {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    }
    *phase = EnginePhase::Connected;
    Ok(stream)
}

pub(in crate::managed_network) fn engine_half_close(input: EngineInputs) -> io::Result<()> {
    let deadline = Deadline::new(input.within)?;
    let before = descriptor_count()?;
    let mut endpoint = Endpoint::reserve(input.maximum)?;
    let mut worker = Worker::spawn(&deadline, move |deadline| {
        let mut phase = EnginePhase::Prepared;
        let mut late = LateWrite::NotAttempted;
        let result = (|| {
            let mut stream = unix_connect(&input.path, &deadline, &mut phase)?;
            endpoint.send(&mut stream, &input.request, &deadline)?;
            phase = EnginePhase::RequestSent;
            if input.half == EngineHalf::ClientFinFirst {
                endpoint.close_write(&stream)?;
                phase = EnginePhase::ClientFin;
            }
            endpoint.receive_to_end(&mut stream, &deadline)?;
            phase = EnginePhase::ResponseEof;
            if !endpoint
                .bytes
                .get(..endpoint.read)
                .is_some_and(|bytes| bytes.ends_with(&input.suffix))
            {
                return Err(invalid("actual engine response suffix differs"));
            }
            if input.half == EngineHalf::HostFinFirst {
                let sent = deadline.io(
                    stream.as_raw_fd(),
                    libc::POLLOUT,
                    || Ok(stream.write(&input.late)),
                )?;
                late = match sent {
                    Ok(n) => LateWrite::Accepted(n),
                    Err(error) => LateWrite::Failed(error.kind(), error.raw_os_error()),
                };
            }
            Ok(())
        })();
        (input.half, phase, late, endpoint, result)
    })?;
    let (half, phase, late, endpoint, result) = worker.join()?;
    drop(worker);
    let after = descriptor_count()?;
    let mut out = io::stdout().lock();
    write!(
        out,
        "managed-tcp-forward {{\"type\":\"EngineHalfClose\",\"order\":\"{half:?}\",\"phase\":\"{phase:?}\",\"read\":{},\"written\":{},\"read_end\":\"{:?}\",\"write_end\":\"{:?}\",\"ok\":{},\"read_hex\":\"",
        endpoint.read,
        endpoint.written,
        endpoint.read_end,
        endpoint.write_end,
        result.is_ok()
    )?;
    for byte in endpoint
        .bytes
        .get(..endpoint.read)
        .ok_or_else(|| invalid("engine retained prefix envelope"))?
    {
        write!(out, "{byte:02x}")?;
    }
    write!(out, "\",\"late_write\":")?;
    match late {
        LateWrite::NotAttempted => write!(out, "{{\"kind\":\"NotAttempted\"}}")?,
        LateWrite::Accepted(n) => write!(out, "{{\"kind\":\"Accepted\",\"bytes\":{n}}}")?,
        LateWrite::Failed(kind, errno) => {
            write!(
                out,
                "{{\"kind\":\"Failed\",\"error_kind\":\"{kind:?}\",\"errno\":"
            )?;
            match errno {
                Some(errno) => write!(out, "{errno}")?,
                None => write!(out, "null")?,
            }
            write!(out, "}}")?;
        }
    }
    write!(
        out,
        ",\"error_kind\":\"{:?}\",\"fd_before\":{before},\"fd_after\":{after},\"worker_joined\":true,\"kernel_return\":\"Unknown\"}}\n",
        result.as_ref().err().map(io::Error::kind)
    )?;
    out.flush()?;
    result?;
    if before != after {
        return Err(invalid("engine probe descriptors did not return"));
    }
    Ok(())
}

#[cfg(test)]
mod native_tests {
    use super::*;

    fn supplied_queue(fd: RawFd, option: i32, supplied: i32, role: &str) {
        assert!(supplied > 0);
        // SAFETY: this test owns fd; exact initialized socket-option integer.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    (&raw const supplied).cast(),
                    size_of_val(&supplied) as _,
                )
            },
            0
        );
        let mut actual = 0i32;
        let mut size = size_of_val(&actual) as libc::socklen_t;
        // SAFETY: initialized writable native option and its exact size.
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    (&raw mut actual).cast(),
                    &raw mut size,
                )
            },
            0
        );
        assert_eq!(size as usize, size_of_val(&actual));
        assert!(actual > 0);
        println!(
            "managed-tcp-forward {{\"type\":\"NativeQueue\",\"role\":\"{role}\",\"option\":{option},\"supplied\":{supplied},\"actual\":{actual}}}"
        );
    }

    fn native_inputs() -> (Inputs, Deadline) {
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
        (input, deadline)
    }

    fn record_flow(forwarded: &Completion, storage: &str, client: SocketAddr, upstream: SocketAddr) {
        let mut out = io::stdout().lock();
        write!(out,"managed-tcp-forward {{\"type\":\"NativeFlow\",\"storage\":\"{storage}\",\"connect\":\"{:?}\",\"end\":",forwarded.connect).unwrap();
        match &forwarded.result {
            Ok(end) => write!(out, "\"{end:?}\"").unwrap(),
            Err(_) => write!(out, "null").unwrap(),
        }
        write!(out, ",\"fault\":").unwrap();
        fault_record(&mut out, &forwarded.result).unwrap();
        write!(out, ",\"abort\":").unwrap();
        abort_record(&mut out, &forwarded.abort).unwrap();
        write!(
            out,
            ",\"client\":\"{client}\",\"upstream\":\"{upstream}\",\"to_backend\":"
        )
        .unwrap();
        progress_record(&mut out, &forwarded.flow.to_backend).unwrap();
        write!(out, ",\"to_client\":").unwrap();
        progress_record(&mut out, &forwarded.flow.to_client).unwrap();
        writeln!(out, "}}").unwrap();
        out.flush().unwrap();
    }

    #[test]
    #[ignore = "private native socket campaign supplies release FD and finite byte/time inputs"]
    fn native_tcp_pump_both_half_closes() {
        let supplied = |name| std::env::var(name).expect("explicit native fixture input");
        let (input, deadline) = native_inputs();
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
        let pressure = std::env::var("SHARDS_NATIVE_WRITE_PRESSURE").ok();
        if let Some(mode) = &pressure {
            assert_eq!(mode, "required");
            let send = supplied("SHARDS_NATIVE_SNDBUF_BYTES").parse().unwrap();
            let receive = supplied("SHARDS_NATIVE_RCVBUF_BYTES").parse().unwrap();
            supplied_queue(upstream.as_raw_fd(), libc::SO_SNDBUF, send, "upstream-send");
            supplied_queue(
                backend_listener.as_raw_fd(),
                libc::SO_RCVBUF,
                receive,
                "backend-listener-receive",
            );
            supplied_queue(
                relay_listener.as_raw_fd(),
                libc::SO_RCVBUF,
                receive,
                "relay-listener-receive",
            );
        }
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
        if pressure.is_some() {
            supplied_queue(
                client.as_raw_fd(),
                libc::SO_SNDBUF,
                supplied("SHARDS_NATIVE_SNDBUF_BYTES").parse().unwrap(),
                "client-send",
            );
        }
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
        println!(
            "managed-tcp-forward {{\"type\":\"NativeJoined\",\"relay_joined\":true,\"backend_joined\":true}}"
        );
        record_flow(&forwarded, &storage, client_tuple.local, backend.0.peer);
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
        if pressure.is_some() {
            assert!(
                forwarded.flow.to_backend.partial_writes > 0,
                "actual short send required"
            );
            assert!(
                forwarded.flow.to_backend.write_would_block > 0,
                "actual send WouldBlock required"
            );
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

    #[test]
    #[ignore = "private finite native prefix/early-backend-completion cancellation campaign"]
    fn native_tcp_cancel_after_backend_receipt() {
        let (input, deadline) = native_inputs();
        assert_eq!(input.scenario, Scenario::ServerFirst);
        let before = descriptor_count().unwrap();
        let [up, down] = input.slot_sizes;
        let slots = Slots::reserve(up, down).unwrap();
        let response = input.greeting.len().checked_add(input.tail.len()).unwrap();
        let request = input.request.len();
        let greeting = input.greeting.len();
        let backend_input = input.duplicate().unwrap();
        let mut backend_endpoint = Endpoint::reserve(request).unwrap();
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
            let result = (|| {
                backend_endpoint.send(&mut stream, &backend_input.greeting, &deadline)?;
                backend_endpoint.receive(&mut stream, &backend_input.request, &deadline)?;
                backend_endpoint.eof(&mut stream, &deadline)
            })();
            backend_endpoint
                .record("NativeBackend", 0, tuple, &result)
                .unwrap();
            (stream, tuple, backend_endpoint, result)
        })
        .unwrap();
        let mut relay = start(
            relay_listener,
            upstream,
            destination,
            slots,
            [request, response],
            &deadline,
        )
        .unwrap();
        let mut client = TcpStream::connect_timeout(&relay_at, deadline.left().unwrap()).unwrap();
        client.set_nonblocking(true).unwrap();
        let tuple = Tuple {
            local: client.local_addr().unwrap(),
            peer: client.peer_addr().unwrap(),
        };
        client_endpoint
            .receive(&mut client, &input.greeting, &deadline)
            .unwrap();
        client_endpoint
            .send(&mut client, &input.request, &deadline)
            .unwrap();
        client_endpoint.close_write(&client).unwrap();
        // Completion returns the actual accepted backend socket still held.
        // The relay must remain pending for its unsent tail, not infer peer EOF
        // from a helper thread ending. No sleep or extra deadline is involved.
        let (backend_stream, backend_tuple, backend_endpoint, backend_result) =
            backend_worker.join().unwrap();
        backend_result.unwrap();
        assert_eq!(backend_endpoint.read, request);
        assert_eq!(backend_endpoint.write_end, End::Open);
        let remaining = deadline.left().unwrap();
        let forwarded = relay.finish().unwrap();
        println!(
            "managed-tcp-forward {{\"type\":\"NativeJoined\",\"relay_joined\":true,\"backend_joined\":true,\"original_remaining_ns\":{}}}",
            remaining.as_nanos()
        );
        record_flow(&forwarded, "caller", tuple.local, backend_tuple.peer);
        let client_result = client_endpoint.eof(&mut client, &deadline);
        client_endpoint
            .record("NativeClient", 0, tuple, &client_result)
            .unwrap();
        client_result.unwrap();
        assert!(
            matches!(forwarded.result, Err(Fault::Cancelled)),
            "{:?}",
            forwarded.result
        );
        assert_eq!(forwarded.flow.to_backend.read, request);
        assert_eq!(forwarded.flow.to_backend.written, request);
        assert_eq!(forwarded.flow.to_backend.half, Half::WriteClosed(SourceEnd::Eof));
        assert_eq!(forwarded.flow.to_client.read, greeting);
        assert_eq!(forwarded.flow.to_client.written, greeting);
        assert_eq!(forwarded.flow.to_client.half, Half::Reading);
        assert_eq!(forwarded.flow.to_client.pending, 0);
        assert_eq!(backend_tuple.peer, forwarded.upstream.unwrap().local);
        drop(backend_stream);
        drop(client);
        drop(relay);
        drop(backend_worker);
        let after = descriptor_count().unwrap();
        assert_eq!(after, before);
        println!(
            "NATIVE_TCP_CANCEL_EXACT backend_request={request} client_greeting={greeting} fd_before={before} fd_after={after} kernel_return=Unknown"
        );
    }

    #[test]
    #[ignore = "finite native direct/mediated reset compatibility with independent prefix receipts"]
    fn native_tcp_reset_matches_direct_peer() {
        let (input, deadline) = native_inputs();
        assert_eq!(input.scenario, Scenario::ServerFirst);
        let mut received_errors = Vec::new();
        let mut native_faults = Vec::new();
        for ordinal in [0, 1] {
            let before = descriptor_count().unwrap();
            let backend_input = input.duplicate().unwrap();
            let mut backend_endpoint = Endpoint::reserve(input.request.len()).unwrap();
            let mut client_endpoint = Endpoint::reserve(input.greeting.len()).unwrap();
            let listener = listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), SocketMode::Native).unwrap();
            let destination = listener.local_addr().unwrap();
            let mut backend = Worker::spawn(&deadline, move |deadline| {
                let (mut stream, peer) = deadline
                    .io(listener.as_raw_fd(), libc::POLLIN, || listener.accept())
                    .unwrap();
                stream.set_nonblocking(true).unwrap();
                let tuple = Tuple {
                    local: stream.local_addr().unwrap(),
                    peer,
                };
                let result = (|| {
                    backend_endpoint.send(&mut stream, &backend_input.greeting, &deadline)?;
                    backend_endpoint.receive(&mut stream, &backend_input.request, &deadline)?;
                    backend_endpoint.eof(&mut stream, &deadline)
                })();
                backend_endpoint
                    .record("ResetBackend", ordinal, tuple, &result)
                    .unwrap();
                (stream, tuple, backend_endpoint, result)
            })
            .unwrap();
            let (target, mut relay) = if ordinal == 0 {
                (destination, None)
            } else {
                let listener =
                    listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), SocketMode::Native).unwrap();
                let at = listener.local_addr().unwrap();
                let upstream = TcpStream::from(
                    bound(
                        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                        libc::SOCK_STREAM,
                        SocketMode::Native,
                    )
                    .unwrap(),
                );
                let [up, down] = input.slot_sizes;
                let slots = Slots::reserve(up, down).unwrap();
                let response = input.greeting.len().checked_add(input.tail.len()).unwrap();
                (
                    at,
                    Some(
                        start(
                            listener,
                            upstream,
                            destination,
                            slots,
                            [input.request.len(), response],
                            &deadline,
                        )
                        .unwrap(),
                    ),
                )
            };
            let mut client = TcpStream::connect_timeout(&target, deadline.left().unwrap()).unwrap();
            client.set_nonblocking(true).unwrap();
            let tuple = Tuple {
                local: client.local_addr().unwrap(),
                peer: client.peer_addr().unwrap(),
            };
            client_endpoint
                .receive(&mut client, &input.greeting, &deadline)
                .unwrap();
            client_endpoint
                .send(&mut client, &input.request, &deadline)
                .unwrap();
            client_endpoint.close_write(&client).unwrap();
            let (stream, backend_tuple, backend_endpoint, result) = backend.join().unwrap();
            result.unwrap();
            assert_eq!(backend_endpoint.read, input.request.len());
            assert_eq!(backend_endpoint.write_end, End::Open);
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            // SAFETY: this fixture owns the accepted socket; zero linger is the
            // native abortive close being compared, never a host policy change.
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_LINGER,
                        (&raw const linger).cast(),
                        size_of_val(&linger) as _,
                    )
                },
                0
            );
            drop(stream);
            let native_reset = if let Some(worker) = &mut relay {
                let forwarded = worker.join().unwrap();
                record_flow(&forwarded, "caller", tuple.local, backend_tuple.peer);
                matches!(
                    forwarded.result,
                    Err(Fault::Native {
                        leg: Leg::Backend,
                        operation: Operation::Read,
                        kind: io::ErrorKind::ConnectionReset,
                        errno: Some(libc::ECONNRESET)
                    })
                )
            } else {
                true
            };
            native_faults.push(native_reset);
            let result = client_endpoint.eof(&mut client, &deadline);
            client_endpoint
                .record("ResetClient", ordinal, tuple, &result)
                .unwrap();
            let kind = result.as_ref().err().map(io::Error::kind);
            received_errors.push(kind);
            println!(
                "managed-tcp-forward {{\"type\":\"NativeReset\",\"ordinal\":{ordinal},\"client_error_kind\":\"{kind:?}\",\"backend_joined\":true,\"relay_joined\":{},\"native_fault_retained\":{native_reset}}}",
                relay.is_some()
            );
            drop(client);
            drop(relay);
            drop(backend);
            let after = descriptor_count().unwrap();
            assert_eq!(after, before);
            println!(
                "managed-tcp-forward {{\"type\":\"NativeResetCleanup\",\"ordinal\":{ordinal},\"fd_before\":{before},\"fd_after\":{after},\"kernel_return\":\"Unknown\"}}"
            );
        }
        assert_eq!(native_faults, [true, true]);
        assert_eq!(
            received_errors,
            [
                Some(io::ErrorKind::ConnectionReset),
                Some(io::ErrorKind::ConnectionReset)
            ],
            "direct and mediated native reset must agree"
        );
    }
}
