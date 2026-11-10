//! Finite UDP compatibility experiment. No forwarding relay, cache, or product policy.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Connected,
    Unconnected,
}

#[derive(Debug)]
pub(in super::super) struct Inputs {
    network: super::Inputs,
    mode: Mode,
}
impl Inputs {
    pub(in super::super) fn parse(
        family: Family,
        origin: &str,
        mode: &str,
        millis: &str,
        mark: &str,
        table: &str,
        priority: &str,
    ) -> io::Result<Self> {
        if !matches!(origin, "local" | "forward" | "local-loopback" | "local-self") {
            return Err(invalid(
                "UDP experiment expects local, forward, local-loopback or local-self",
            ));
        }
        Ok(Self {
            network: super::Inputs::parse(family, origin, millis, mark, table, priority)?,
            mode: match mode {
                "connected" => Mode::Connected,
                "unconnected" => Mode::Unconnected,
                _ => return Err(invalid("UDP client mode must be connected or unconnected")),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exchange {
    Binary,
    Empty,
}
impl Exchange {
    // Two semantic cases, not a capacity or learned runtime limit.
    const SCRIPT: [Self; 2] = [Self::Binary, Self::Empty];
    fn request(self) -> &'static [u8] {
        match self {
            Self::Binary => SENT,
            Self::Empty => b"",
        }
    }
    fn response(self, role: Reply) -> &'static [u8] {
        match self {
            Self::Binary => role.bytes(),
            Self::Empty => b"",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    ReserveReplyTuple,
    BindClient,
    ConnectDefaultPeer,
    SendDatagram,
    ReceiveDatagram,
    ValidateDatagram,
    ReplySend,
    Record,
}
#[derive(Debug)]
enum Fault {
    Io(io::Error),
    Metadata(UdpMetadataError),
    Payload,
    Tuple,
    PartialSend,
    CountOverflow,
}
impl Fault {
    fn tag(&self) -> &'static str {
        match self {
            Self::Io(_) => "Io",
            Self::Metadata(_) => "Metadata",
            Self::Payload => "Payload",
            Self::Tuple => "Tuple",
            Self::PartialSend => "PartialSend",
            Self::CountOverflow => "CountOverflow",
        }
    }
    fn io(&self) -> Option<&io::Error> {
        match self {
            Self::Io(e) | Self::Metadata(UdpMetadataError::Peer(e) | UdpMetadataError::Original(e)) => {
                Some(e)
            }
            _ => None,
        }
    }
    fn into_io(self) -> io::Error {
        match self {
            Self::Io(e) => e,
            Self::Metadata(e) => e.into_io(),
            Self::Payload => invalid("UDP fixture datagram differs or was truncated"),
            Self::Tuple => invalid("UDP fixture original/peer tuple differs"),
            Self::PartialSend => invalid("UDP fixture send was partial"),
            Self::CountOverflow => invalid("UDP fixture arrival count overflow"),
        }
    }
}
fn validate(
    arrival: UdpArrival,
    expected: &[u8],
    original: SocketAddr,
    peer: Option<SocketAddr>,
) -> Result<UdpMetadata, Fault> {
    if !arrival.payload_matches(expected) {
        return Err(Fault::Payload);
    }
    let metadata = arrival.metadata.map_err(Fault::Metadata)?;
    if metadata.original != Some(original) || peer.is_some_and(|peer| peer != metadata.peer) {
        return Err(Fault::Tuple);
    }
    Ok(metadata)
}

fn hex(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    for byte in bytes {
        write!(out, "{byte:02x}")?;
    }
    Ok(())
}
fn arrival_record(
    role: &str,
    step: Exchange,
    ordinal: usize,
    local: SocketAddr,
    arrival: &UdpArrival,
) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(
        out,
        "managed-intercept {{\"type\":\"UdpArrival\",\"role\":\"{role}\",\"step\":\"{step:?}\",\"ordinal\":{ordinal},\"local\":\"{local}\",\"reported\":{},\"flags\":{},\"copied_hex\":\"",
        arrival.reported, arrival.flags
    )?;
    hex(&mut out, &arrival.copied)?;
    write!(out, "\",\"metadata\":\"")?;
    match &arrival.metadata {
        Ok(metadata) => {
            write!(out, "Known\",\"peer\":\"{}\",\"original\":", metadata.peer)?;
            match metadata.original {
                Some(at) => write!(out, "\"{at}\"")?,
                None => write!(out, "null")?,
            }
        }
        Err(error) => write!(out, "{}\",\"peer\":null,\"original\":null", error.tag())?,
    }
    writeln!(out, "}}")?;
    out.flush()
}
fn fault_record(role: &str, stage: Stage, fault: &Fault) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(
        out,
        "managed-intercept {{\"type\":\"UdpError\",\"role\":\"{role}\",\"stage\":\"{stage:?}\",\"cause\":\"{}\",\"kind\":",
        fault.tag()
    )?;
    match fault.io() {
        Some(error) => write!(out, "\"{:?}\"", error.kind())?,
        None => write!(out, "null")?,
    }
    write!(out, ",\"errno\":")?;
    match fault.io().and_then(io::Error::raw_os_error) {
        Some(errno) => write!(out, "{errno}")?,
        None => write!(out, "null")?,
    }
    writeln!(out, "}}")?;
    out.flush()
}

#[derive(Default)]
struct DatagramReport {
    arrivals: usize,
    matched: usize,
    empty: usize,
}
impl DatagramReport {
    fn consume(&mut self, arrival: &UdpArrival) -> Result<(), Fault> {
        self.arrivals = self.arrivals.checked_add(1).ok_or(Fault::CountOverflow)?;
        if arrival.reported == 0 {
            self.empty = self.empty.checked_add(1).ok_or(Fault::CountOverflow)?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug)]
enum Terminal {
    ScriptComplete,
    Cancelled,
}
struct Completion {
    report: DatagramReport,
    stage: Stage,
    result: Result<Terminal, Fault>,
}
struct DatagramWorker {
    cancel: OwnedFd,
    state: WorkerState,
}
enum WorkerState {
    Running(JoinHandle<Completion>),
    Joined,
}
impl DatagramWorker {
    fn start(
        receiver: UdpSocket,
        sender: UdpSocket,
        destination: SocketAddr,
        role: Reply,
        deadline: &Deadline,
    ) -> io::Result<Self> {
        // SAFETY: eventfd returns a new descriptor, owned immediately below.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is this newly created unique eventfd.
        let cancel = unsafe { OwnedFd::from_raw_fd(fd) };
        let receiver_cancel = cancel.try_clone()?;
        let deadline = Deadline {
            at: deadline.at,
            cancel: Some(receiver_cancel.as_raw_fd()),
        };
        let thread = std::thread::Builder::new().spawn(move || {
            let mut report = DatagramReport::default();
            let mut step = Exchange::Binary;
            let mut stage = Stage::ReceiveDatagram;
            let result = (|| loop {
                stage = Stage::ReceiveDatagram;
                let arrival = match udp_arrival(&receiver, SENT.len(), &deadline) {
                    Ok(arrival) => arrival,
                    Err(error) => {
                        if error.kind() == io::ErrorKind::ConnectionAborted
                            && cancellation_observed(&receiver_cancel)
                        {
                            return Ok(Terminal::Cancelled);
                        }
                        return Err(Fault::Io(error));
                    }
                };
                // Count a consumed datagram before any payload/metadata/report failure.
                report.consume(&arrival)?;
                stage = Stage::Record;
                arrival_record(
                    match role {
                        Reply::Backend => "Backend",
                        Reply::Relay => "Relay",
                    },
                    step,
                    report.arrivals,
                    receiver.local_addr().map_err(Fault::Io)?,
                    &arrival,
                )
                .map_err(Fault::Io)?;
                stage = Stage::ValidateDatagram;
                let metadata = validate(arrival, step.request(), destination, None)?;
                report.matched = report.matched.checked_add(1).ok_or(Fault::CountOverflow)?;
                stage = Stage::ReplySend;
                let bytes = step.response(role);
                let n = deadline
                    .io(sender.as_raw_fd(), libc::POLLOUT, || {
                        sender.send_to(bytes, metadata.peer)
                    })
                    .map_err(Fault::Io)?;
                if n != bytes.len() {
                    return Err(Fault::PartialSend);
                }
                match (step, role) {
                    (Exchange::Binary, _) => step = Exchange::Empty,
                    (Exchange::Empty, Reply::Backend) => step = Exchange::Binary,
                    (Exchange::Empty, Reply::Relay) => return Ok(Terminal::ScriptComplete),
                }
            })();
            Completion {
                report,
                stage,
                result,
            }
        })?;
        Ok(Self {
            cancel,
            state: WorkerState::Running(thread),
        })
    }
    fn wake(&self) {
        let word = 1u64;
        // SAFETY: eventfd consumes one uint64. Original deadline also bounds cleanup.
        let _ = unsafe {
            libc::write(
                self.cancel.as_raw_fd(),
                (&raw const word).cast(),
                size_of_val(&word),
            )
        };
    }
    fn finish(&mut self) -> io::Result<Completion> {
        self.wake();
        match std::mem::replace(&mut self.state, WorkerState::Joined) {
            WorkerState::Running(thread) => {
                thread.join().map_err(|_| io::Error::other("UDP worker panicked"))
            }
            WorkerState::Joined => Err(invalid("UDP worker already joined")),
        }
    }
}
impl Drop for DatagramWorker {
    fn drop(&mut self) {
        if matches!(self.state, WorkerState::Running(_)) {
            let _ = self.finish();
        }
    }
}
fn cancellation_observed(cancel: &OwnedFd) -> bool {
    let mut word = 0u64;
    // SAFETY: nonblocking eventfd writes into exactly one initialized uint64.
    unsafe {
        libc::read(cancel.as_raw_fd(), (&raw mut word).cast(), size_of_val(&word))
            == size_of_val(&word) as isize
    }
}

struct ClientCompletion {
    stage: Stage,
    result: Result<UdpSocket, Fault>,
}
fn client(
    namespace: &Namespace,
    source: IpAddr,
    destination: SocketAddr,
    mode: Mode,
    expected: Reply,
    deadline: &Deadline,
) -> io::Result<ClientCompletion> {
    in_namespace(Some(&namespace.handle), || {
        let mut stage = Stage::BindClient;
        let result = (|| {
            // Bind the authoritative fixture address so unconnected local_addr is
            // an actual source tuple, rather than a wildcard inferred after send.
            let socket = udp_socket(SocketAddr::from((source, 0)), SocketMode::Native).map_err(Fault::Io)?;
            let local = socket.local_addr().map_err(Fault::Io)?;
            if mode == Mode::Connected {
                stage = Stage::ConnectDefaultPeer;
                socket.connect(destination).map_err(Fault::Io)?;
            }
            for (index, step) in Exchange::SCRIPT.into_iter().enumerate() {
                stage = Stage::SendDatagram;
                let bytes = step.request();
                let n = deadline
                    .io(socket.as_raw_fd(), libc::POLLOUT, || match mode {
                        Mode::Connected => socket.send(bytes),
                        Mode::Unconnected => socket.send_to(bytes, destination),
                    })
                    .map_err(Fault::Io)?;
                if n != bytes.len() {
                    return Err(Fault::PartialSend);
                }
                stage = Stage::ReceiveDatagram;
                let arrival = udp_arrival(
                    &socket,
                    Reply::Backend.bytes().len().max(Reply::Relay.bytes().len()),
                    deadline,
                )
                .map_err(Fault::Io)?;
                stage = Stage::Record;
                arrival_record(
                    match expected {
                        Reply::Backend => "BaselineClient",
                        Reply::Relay => "InterceptedClient",
                    },
                    step,
                    index + 1,
                    local,
                    &arrival,
                )
                .map_err(Fault::Io)?;
                stage = Stage::ValidateDatagram;
                validate(arrival, step.response(expected), local, Some(destination))?;
            }
            Ok(socket)
        })();
        Ok(ClientCompletion { stage, result })
    })
}
fn client_result(role: &str, completion: &ClientCompletion) -> io::Result<()> {
    if let Err(error) = &completion.result {
        fault_record(role, completion.stage, error)?;
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"UdpClientOutcome\",\"role\":\"{role}\",\"stage\":\"{:?}\",\"outcome\":\"{}\"}}",
        completion.stage,
        if completion.result.is_ok() {
            "ScriptComplete"
        } else {
            "Error"
        }
    )?;
    out.flush()
}
fn worker_record(role: Reply, completed: &io::Result<Completion>) -> io::Result<()> {
    let mut out = io::stdout().lock();
    match completed {
        Ok(completed) => {
            write!(
                out,
                "managed-intercept {{\"type\":\"UdpWorkerCompletion\",\"role\":\"{role:?}\",\"joined\":true,\"arrivals\":{},\"matched\":{},\"empty\":{},\"terminal\":\"",
                completed.report.arrivals, completed.report.matched, completed.report.empty
            )?;
            match &completed.result {
                Ok(terminal) => write!(out, "{terminal:?}")?,
                Err(_) => write!(out, "Error")?,
            }
            writeln!(out, "\"}}")?;
        }
        Err(_) => writeln!(
            out,
            "managed-intercept {{\"type\":\"UdpWorkerCompletion\",\"role\":\"{role:?}\",\"joined\":false,\"terminal\":\"JoinError\"}}"
        )?,
    }
    out.flush()?;
    drop(out);
    if let Ok(completed) = completed
        && let Err(error) = &completed.result
    {
        fault_record(
            match role {
                Reply::Backend => "Backend",
                Reply::Relay => "Relay",
            },
            completed.stage,
            error,
        )?;
    }
    Ok(())
}

pub(in super::super) fn run(input: Inputs) -> io::Result<()> {
    if std::process::id() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "UDP experiment requires direct guest PID1",
        ));
    }
    let deadline = Deadline::new(input.network.within)?;
    let [router_ip, backend_ip, _, client_ip] = addresses(input.network.family);
    let [router, backend, client_ns] = prepare(&input.network, &deadline)?;
    let loopback: IpAddr = match input.network.family {
        Family::Ipv4 => Ipv4Addr::LOCALHOST.into(),
        Family::Ipv6 => Ipv6Addr::LOCALHOST.into(),
    };
    let (origin, source, backend_namespace, backend_address) = match input.network.origin {
        Origin::Local => (&router, router_ip, &backend, backend_ip),
        Origin::Forward => (&client_ns, client_ip, &backend, backend_ip),
        Origin::LocalLoopback => (&router, loopback, &router, loopback),
        Origin::LocalSelf => (&router, router_ip, &router, router_ip),
    };
    let backend_socket = in_namespace(Some(&backend_namespace.handle), || {
        udp_socket(SocketAddr::from((backend_address, 0)), SocketMode::Native)
    })?;
    let destination = backend_socket.local_addr()?;
    {
        // These facts precede the reservation syscall boundary. No baseline or
        // datagram worker exists here; a native reservation failure cannot prove
        // interception or worker cleanup. Held namespace objects name the roles.
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "managed-intercept {{\"type\":\"UdpSetup\",\"stage\":\"{:?}\",\"family\":\"{:?}\",\"origin\":\"{:?}\",\"mode\":\"{:?}\",\"destination\":\"{destination}\",\"client_inode\":{},\"backend_inode\":{},\"relay_inode\":{},\"baseline\":\"NotStarted\",\"backend_worker\":\"NotStarted\",\"relay_worker\":\"NotStarted\"}}",
            Stage::ReserveReplyTuple,
            input.network.family,
            input.network.origin,
            input.mode,
            origin.inode()?,
            backend_namespace.inode()?,
            router.inode()?
        )?;
        out.flush()?;
    }
    // Reserve the original reply tuple BEFORE wildcard port allocation. Do not
    // connect it: pinned nf_tproxy IPv4/IPv6 exclude unconnected sockets from the
    // established lookup; connecting it could divert later requests to this FD.
    let sender = match in_namespace(Some(&router.handle), || {
        udp_socket(destination, SocketMode::Transparent)
    }) {
        Ok(sender) => sender,
        Err(error) => {
            let fault = Fault::Io(error);
            fault_record("ReplySocket", Stage::ReserveReplyTuple, &fault)?;
            return Err(fault.into_io());
        }
    };
    let unspecified: IpAddr = match input.network.family {
        Family::Ipv4 => Ipv4Addr::UNSPECIFIED.into(),
        Family::Ipv6 => Ipv6Addr::UNSPECIFIED.into(),
    };
    let receiver = in_namespace(Some(&router.handle), || {
        udp_socket(SocketAddr::from((unspecified, 0)), SocketMode::Transparent)
    })?;
    let relay_port = receiver.local_addr()?.port();
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"UdpTarget\",\"family\":\"{:?}\",\"origin\":\"{:?}\",\"mode\":\"{:?}\",\"destination\":\"{destination}\",\"relay_port\":{relay_port},\"reply_local\":\"{}\",\"client_inode\":{},\"backend_inode\":{},\"relay_inode\":{}}}",
        input.network.family,
        input.network.origin,
        input.mode,
        sender.local_addr()?,
        origin.inode()?,
        backend_namespace.inode()?,
        router.inode()?
    )?;
    out.flush()?;
    drop(out);
    let backend_sender = backend_socket.try_clone()?;
    let mut backend_worker = DatagramWorker::start(
        backend_socket,
        backend_sender,
        destination,
        Reply::Backend,
        &deadline,
    )?;
    let mut relay_worker = None;
    let result: io::Result<()> = (|| {
        let baseline = client(origin, source, destination, input.mode, Reply::Backend, &deadline)?;
        client_result("BaselineClient", &baseline)?;
        // Hold the successful baseline socket through the second phase so its
        // source port cannot be reused for the fresh intercepted client.
        let _baseline = baseline.result.map_err(Fault::into_io)?;
        in_namespace(Some(&router.handle), || {
            router
                .route
                .policy(interface(c"lo")?, &input.network, destination, &deadline)?;
            Netlink::open(libc::NETLINK_NETFILTER)?.interception(
                &input.network,
                destination,
                relay_port,
                wire::Protocol::Udp,
                &deadline,
            )
        })?;
        relay_worker = Some(DatagramWorker::start(
            receiver,
            sender,
            destination,
            Reply::Relay,
            &deadline,
        )?);
        let intercepted = client(origin, source, destination, input.mode, Reply::Relay, &deadline)?;
        client_result("InterceptedClient", &intercepted)?;
        let _intercepted = intercepted.result.map_err(Fault::into_io)?;
        Ok(())
    })();
    // Every started worker is joined before propagating client/setup/I/O failure.
    let relay = relay_worker.as_mut().map(DatagramWorker::finish);
    let backend = backend_worker.finish();
    if let Some(relay) = &relay {
        worker_record(Reply::Relay, relay)?;
    }
    worker_record(Reply::Backend, &backend)?;
    result?;
    let relay = relay.ok_or_else(|| invalid("UDP relay worker never started"))??;
    let backend = backend?;
    relay.result.map_err(Fault::into_io)?;
    backend.result.map_err(Fault::into_io)?;
    let expected = Exchange::SCRIPT.len();
    if relay.report.arrivals != expected
        || relay.report.matched != expected
        || relay.report.empty != 1
        || backend.report.arrivals != expected
        || backend.report.matched != expected
        || backend.report.empty != 1
    {
        return Err(invalid("UDP actual arrival/validation counts differ"));
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "managed-intercept {{\"type\":\"UdpIntercepted\",\"joined\":true,\"after_window\":\"Unverified\"}}"
    )?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inputs_have_explicit_local_roles_and_do_not_inherit_tcp_absence() {
        for family in [Family::Ipv4, Family::Ipv6] {
            for origin in ["local", "forward", "local-loopback", "local-self"] {
                for mode in ["connected", "unconnected"] {
                    assert!(Inputs::parse(family, origin, mode, "1000", "1", "1000", "100").is_ok());
                }
            }
        }
        for origin in ["forward-relay-absent", "unknown"] {
            assert!(Inputs::parse(Family::Ipv4, origin, "connected", "1000", "1", "1000", "100").is_err());
        }
        assert!(Inputs::parse(Family::Ipv6, "forward", "tcp", "1000", "1", "1000", "100").is_err());
    }
    #[test]
    fn empty_is_a_consumed_datagram_and_truncation_or_unknown_metadata_never_matches() {
        let at = SocketAddr::from((BACKEND, 42));
        let arrival = |reported, flags, copied, metadata| UdpArrival {
            reported,
            flags,
            copied,
            metadata,
        };
        let known = || {
            Ok(UdpMetadata {
                peer: at,
                original: Some(at),
            })
        };
        let empty = arrival(0, 0, vec![], known());
        let mut report = DatagramReport::default();
        report.consume(&empty).unwrap();
        assert_eq!((report.arrivals, report.empty), (1, 1));
        assert!(validate(empty, b"", at, Some(at)).is_ok());
        assert!(validate(arrival(0, libc::MSG_CTRUNC, vec![], known()), b"", at, Some(at)).is_err());
        assert!(
            validate(
                arrival(0, 0, vec![], Err(UdpMetadataError::ShortOriginal)),
                b"",
                at,
                Some(at)
            )
            .is_err()
        );
        assert!(
            validate(
                arrival(
                    0,
                    0,
                    vec![],
                    Ok(UdpMetadata {
                        peer: at,
                        original: None
                    })
                ),
                b"",
                at,
                Some(at)
            )
            .is_err()
        );
        let oversized = arrival(SENT.len() + 1, libc::MSG_TRUNC, SENT.to_vec(), known());
        report.consume(&oversized).unwrap();
        assert_eq!((report.arrivals, report.empty), (2, 1));
        assert!(validate(oversized, SENT, at, None).is_err());
        let other = SocketAddr::from((BACKEND, 43));
        assert!(validate(arrival(0, 0, vec![], known()), b"", other, None).is_err());
        assert!(validate(arrival(0, 0, vec![], known()), b"", at, Some(other)).is_err());
    }
}
