//! Native code-mode results through the existing owner-private capture transport.
use super::*;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};

struct Inputs {
    case: Duration,
    call: Duration,
    cleanup: Duration,
    timeout_ms: u64,
    cancel_work_ms: u64,
    bytes: usize,
    records: PathBuf,
}

impl Inputs {
    fn read() -> Self {
        let text = |key: &str| {
            std::env::var(format!("SHARDS_BROKER_EFFECTS_{key}"))
                .unwrap_or_else(|_| panic!("supply SHARDS_BROKER_EFFECTS_{key}"))
        };
        let number = |key: &str| text(key).parse::<u64>().unwrap();
        let inputs = Self {
            case: Duration::from_millis(number("CASE_MS")),
            call: Duration::from_millis(number("CALL_MS")),
            cleanup: Duration::from_millis(number("CLEANUP_MS")),
            timeout_ms: number("TIMEOUT_MS"),
            cancel_work_ms: number("CANCEL_WORK_MS"),
            bytes: usize::try_from(number("CAPTURE_BYTES")).unwrap(),
            records: PathBuf::from(text("RECORDS")),
        };
        assert!(!inputs.call.is_zero() && !inputs.cleanup.is_zero());
        assert!(inputs.case > inputs.cleanup && inputs.call <= inputs.case - inputs.cleanup);
        assert!(inputs.timeout_ms > 0 && inputs.cancel_work_ms > 300);
        assert!(inputs.bytes > 0 && inputs.records.is_absolute());
        std::fs::create_dir(&inputs.records).unwrap();
        inputs
    }

    fn write(&self, name: &str, value: Value) {
        std::fs::write(
            self.records.join(name),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }

    fn command(&self, fixture: &Fixture, name: &str, args: &[&str], until: Instant) -> common::Run {
        let left = until.saturating_duration_since(Instant::now()).min(self.call);
        assert!(!left.is_zero(), "original deadline before {name}");
        let result = fixture.command(args, left);
        self.write(name, json!({"args":args,"status":result.status,"signal":result.signal,"stdout":result.stdout,"stderr":result.stderr}));
        assert_eq!(result.status, Some(0), "{result}");
        result
    }
}

#[derive(Default)]
struct Capture {
    wire: u64,
    bytes: Vec<u8>,
    state: StreamState,
}

impl Capture {
    fn accept(&mut self, event: Event, observer: &Observer, inputs: &Inputs) {
        let Event::Record(offset, chunk) = event else {
            panic!("live owner stream required: {event:?}");
        };
        std::fs::write(
            inputs.records.join(format!(
                "wire-{}-{}-{offset}.bin",
                observer.scope.member, observer.scope.channel as u8
            )),
            &chunk,
        )
        .unwrap();
        assert!(!chunk.is_empty());
        assert_eq!(offset, self.wire);
        self.wire = self
            .wire
            .checked_add(u64::try_from(chunk.len()).unwrap())
            .unwrap();
        let record = Record::decode(&chunk).unwrap();
        assert_eq!(record.member, observer.scope.member);
        assert_eq!(record.channel, observer.scope.channel);
        self.state.accept(record).unwrap();
        match record.event {
            CaptureEvent::Open(label) => assert_eq!(label, declared_label(record.member)),
            CaptureEvent::Data(bytes) => {
                assert!(self.bytes.len().checked_add(bytes.len()).unwrap() <= inputs.bytes);
                self.bytes.extend_from_slice(bytes);
            }
            other => panic!("actor should remain live at this cut: {other:?}"),
        }
        std::fs::write(
            inputs
                .records
                .join(format!("member-{}-{}.bin", record.member, record.channel as u8)),
            &self.bytes,
        )
        .unwrap();
        observer
            .sender
            .send(Command::Handler(Ask::Ack(self.wire)))
            .unwrap();
    }

    #[track_caller]
    fn complete(&mut self, observer: &Observer, inputs: &Inputs, until: Instant) -> String {
        let until = until.min(Instant::now().checked_add(inputs.call).unwrap());
        loop {
            self.accept(observer.next(until), observer, inputs);
            let text = std::str::from_utf8(&self.bytes).unwrap();
            if text.ends_with('\n')
                && text
                    .lines()
                    .last()
                    .is_some_and(|line| line.starts_with("confined net="))
            {
                return text.to_string();
            }
        }
    }

    fn snapshot(&mut self, observer: &Observer, inputs: &Inputs) -> Value {
        // A nonblocking receipt drain establishes only this observation cut, never EOF.
        while let Ok(event) = observer.events.try_recv() {
            self.accept(event, observer, inputs);
        }
        json!({"member":observer.scope.member,"channel":observer.scope.channel as u8,"generation":observer.scope.generation,"wire_ack_enqueued":self.wire,"payload_received":self.bytes.len(),"stream_state":format!("{:?}",self.state),"terminal_observed":self.state.terminal(),"scope":"authenticated parent Observer subscription; raw wire retained","absence_limit":"no future stderr/EOF/complete kernel return inferred"})
    }
}

/// This host's IPv4 addresses: getifaddrs(3)'s; on Windows each up adapter's, its loopback
/// among them (shards_net::bridge::host_addresses, GetAdaptersAddresses). The guest's egress
/// reaches the gate from one of them, the kernel's choice of source: on a host with several,
/// not always the gate's own (adacachy: a connection to 192.168.1.90 came from 192.168.1.77).
#[cfg(windows)]
fn host_addresses() -> std::collections::BTreeSet<IpAddr> {
    shards_net::bridge::host_addresses()
        .into_iter()
        .filter(IpAddr::is_ipv4)
        .collect()
}

#[cfg(unix)]
fn host_addresses() -> std::collections::BTreeSet<IpAddr> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs(3) fills `list`, freed below by freeifaddrs(3) alone.
    let status = unsafe { libc::getifaddrs(&raw mut list) };
    assert_eq!(status, 0, "getifaddrs: {}", std::io::Error::last_os_error());
    let mut addresses = std::collections::BTreeSet::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: a node of the list getifaddrs(3) returned, live until freeifaddrs(3).
        let node = unsafe { &*entry };
        // SAFETY: a non-null ifa_addr points at a sockaddr of its sa_family.
        if !node.ifa_addr.is_null() && i32::from(unsafe { (*node.ifa_addr).sa_family }) == libc::AF_INET {
            // SAFETY: an AF_INET sockaddr is a sockaddr_in.
            let v4 = unsafe { &*node.ifa_addr.cast::<libc::sockaddr_in>() };
            addresses.insert(IpAddr::from(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))));
        }
        entry = node.ifa_next;
    }
    // SAFETY: the list getifaddrs(3) returned, freed once.
    unsafe { libc::freeifaddrs(list) };
    addresses
}

/// The first connection on `listener` whose peer `ours` admits, before `until`. A peer it
/// does not is closed unanswered and returned among the refused.
/// The first connection on `listener` from `ours`, the strangers before it refused: within
/// the campaign's deadline `until`, or with none where the connections were made before the
/// call (an order, the gate's own unit test).
fn accept_from(
    listener: &TcpListener,
    ours: impl Fn(IpAddr) -> bool,
    until: Option<Instant>,
) -> (TcpStream, SocketAddr, Vec<SocketAddr>) {
    listener.set_nonblocking(true).unwrap();
    let mut refused = Vec::new();
    loop {
        // poll(2)'s and WSAPoll's -1: no timeout.
        let ms = match until {
            Some(until) => {
                let left = until.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "original gate deadline; refused peers {refused:?}"
                );
                i32::try_from(left.as_millis().checked_add(1).unwrap()).unwrap()
            }
            None => -1,
        };
        if !connection_waits(listener, ms) {
            continue;
        }
        match listener.accept() {
            Ok((peer, address)) if ours(address.ip()) => return (peer, address, refused),
            Ok((stranger, address)) => {
                drop(stranger);
                refused.push(address);
            }
            // A connection reset between readiness and accept leaves none to take.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("gate accept: {error}"),
        }
    }
}

/// Waits up to `ms` for a connection on `listener` to take (poll(2)); false where a signal
/// cut the wait short.
#[cfg(unix)]
fn connection_waits(listener: &TcpListener, ms: i32) -> bool {
    use std::os::fd::AsRawFd as _;
    let mut ready = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd borrowing the live owned listener.
    let result = unsafe { libc::poll(&raw mut ready, 1, ms) };
    if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
        return false;
    }
    assert_eq!(
        result,
        1,
        "owned gate readiness: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(ready.revents, libc::POLLIN);
    true
}

/// [`connection_waits`] on Windows: WSAPoll, whose listener with a connection to take reads
/// as normal data ready (POLLRDNORM).
#[cfg(windows)]
fn connection_waits(listener: &TcpListener, ms: i32) -> bool {
    use std::os::windows::io::AsRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAPOLLFD, WSAPoll};
    let mut ready = WSAPOLLFD {
        fd: usize::try_from(listener.as_raw_socket()).unwrap(),
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: one initialized WSAPOLLFD naming the live owned listener.
    let result = unsafe { WSAPoll(&raw mut ready, 1, ms) };
    assert_eq!(
        result,
        1,
        "owned gate readiness: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(ready.revents, POLLRDNORM);
    true
}

fn release_gate(listener: &TcpListener, inputs: &Inputs, until: Instant) {
    let until = until.min(Instant::now().checked_add(inputs.call).unwrap());
    let host = host_addresses();
    let (mut peer, address, refused) = accept_from(listener, |ip| host.contains(&ip), Some(until));
    peer.set_write_timeout(Some(
        until.saturating_duration_since(Instant::now()).min(inputs.call),
    ))
    .unwrap();
    peer.write_all(b"after-helper-results\n").unwrap();
    peer.shutdown(std::net::Shutdown::Write).unwrap();
    drop(peer);
    inputs.write("gate-release.json", json!({"peer":address.to_string(),"gate":listener.local_addr().unwrap().to_string(),"refused_peers_not_of_this_host":refused.iter().map(ToString::to_string).collect::<Vec<_>>(),"after":"helper canonical result footer and independent main exec","bytes":"after-helper-results\n","socket_drop_close_return":"unobserved; later harness hear result is delivery receipt"}));
}

/// A TCP connection to `to` from `source`, which std cannot bind before it connects.
#[cfg(unix)]
fn connect_from(source: Ipv4Addr, to: SocketAddr) -> TcpStream {
    use std::os::fd::FromRawFd as _;
    let SocketAddr::V4(to) = to else {
        panic!("an IPv4 gate");
    };
    let address = |ip: Ipv4Addr, port: u16| {
        // SAFETY: sockaddr_in is plain C data, for which all zeroes is a value.
        let mut at: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        // A BSD sockaddr says its own length (macOS's netinet/in.h); Linux's has no such field.
        #[cfg(target_os = "macos")]
        {
            at.sin_len = u8::try_from(size_of::<libc::sockaddr_in>()).unwrap();
        }
        at.sin_family = libc::sa_family_t::try_from(libc::AF_INET).unwrap();
        at.sin_port = port.to_be();
        at.sin_addr.s_addr = u32::from(ip).to_be();
        at
    };
    let size = libc::socklen_t::try_from(size_of::<libc::sockaddr_in>()).unwrap();
    // Close-on-exec as it is made, on Linux; macOS's socket(2) takes no such flag, and the
    // descriptor is made so at once after, as std makes its own there.
    #[cfg(not(target_os = "macos"))]
    // SAFETY: socket(2) returning a new descriptor, owned by the stream below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    #[cfg(target_os = "macos")]
    // SAFETY: socket(2) returning a new descriptor, owned by the stream below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the descriptor socket(2) just returned, owned once.
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    #[cfg(target_os = "macos")]
    {
        // SAFETY: fcntl(2) setting a flag of the descriptor the stream owns.
        let set = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        assert_ne!(set, -1, "{}", std::io::Error::last_os_error());
    }
    let from = address(source, 0);
    // SAFETY: an initialized sockaddr_in of its exact size.
    let bound = unsafe { libc::bind(fd, (&raw const from).cast(), size) };
    assert_eq!(bound, 0, "{}", std::io::Error::last_os_error());
    let peer = address(*to.ip(), to.port());
    // SAFETY: an initialized sockaddr_in of its exact size; a blocking connect.
    let connected = unsafe { libc::connect(fd, (&raw const peer).cast(), size) };
    assert_eq!(connected, 0, "{}", std::io::Error::last_os_error());
    stream
}

/// [`connect_from`] on Windows: Winsock's socket, kept from the processes this binary's tests
/// start, as std makes its own (WSA_FLAG_NO_HANDLE_INHERIT). Winsock is started by std's
/// first socket, the gate's.
#[cfg(windows)]
fn connect_from(source: Ipv4Addr, to: SocketAddr) -> TcpStream {
    use std::os::windows::io::FromRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, IN_ADDR, IN_ADDR_0, INVALID_SOCKET, IPPROTO_TCP, SOCK_STREAM, SOCKADDR, SOCKADDR_IN,
        WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSASocketW, bind, connect,
    };
    let SocketAddr::V4(to) = to else {
        panic!("an IPv4 gate");
    };
    let address = |ip: Ipv4Addr, port: u16| SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: port.to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from(ip).to_be(),
            },
        },
        sin_zero: [0; 8],
    };
    let size = i32::try_from(size_of::<SOCKADDR_IN>()).unwrap();
    // SAFETY: WSASocketW making a new socket from no protocol info, owned by the stream below.
    let socket = unsafe {
        WSASocketW(
            i32::from(AF_INET),
            SOCK_STREAM,
            IPPROTO_TCP,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    assert_ne!(socket, INVALID_SOCKET, "{}", std::io::Error::last_os_error());
    // SAFETY: the socket WSASocketW just returned, owned once.
    let stream = unsafe { TcpStream::from_raw_socket(u64::try_from(socket).unwrap()) };
    let from = address(source, 0);
    // SAFETY: an initialized SOCKADDR_IN of its exact size.
    let bound = unsafe { bind(socket, (&raw const from).cast::<SOCKADDR>(), size) };
    assert_eq!(bound, 0, "{}", std::io::Error::last_os_error());
    let peer = address(*to.ip(), to.port());
    // SAFETY: an initialized SOCKADDR_IN of its exact size; a blocking connect.
    let connected = unsafe { connect(socket, (&raw const peer).cast::<SOCKADDR>(), size) };
    assert_eq!(connected, 0, "{}", std::io::Error::last_os_error());
    stream
}

/// A source address of this host other than 127.0.0.1, standing in for a peer on another host:
/// 127.0.0.2, which Linux's and Windows' loopback serve as they serve all of 127.0.0.0/8. macOS's
/// lo0 serves 127.0.0.1 alone, without an alias its root adds: there, this host's address on its
/// route out, where it has one.
fn stranger() -> Option<Ipv4Addr> {
    if cfg!(target_os = "macos") {
        common::routable_v4()
    } else {
        Some(Ipv4Addr::new(127, 0, 0, 2))
    }
}

#[test]
fn the_gate_releases_only_a_peer_of_this_host() {
    use std::io::Read as _;
    let gate = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let at = gate.local_addr().unwrap();
    // A stranger stands in for a peer on another host, 127.0.0.1 for this host's own.
    let elsewhere = stranger().map(|source| connect_from(source, at));
    let own = connect_from(Ipv4Addr::LOCALHOST, at);
    let ours = |ip: IpAddr| ip == IpAddr::from(Ipv4Addr::LOCALHOST);
    // Both connections made before it: its accept is an order, no bound.
    let (_peer, address, refused) = accept_from(&gate, ours, None);
    assert_eq!(address, own.local_addr().unwrap());
    match elsewhere {
        Some(mut elsewhere) => {
            assert_eq!(refused, [elsewhere.local_addr().unwrap()]);
            let mut byte = [0];
            assert_eq!(
                elsewhere.read(&mut byte).unwrap(),
                0,
                "a refused peer gets nothing"
            );
        }
        None => {
            let _ = writeln!(
                std::io::stderr(),
                "SKIP: a refused peer: this host has no address but its loopback's to stand in for another's"
            );
            assert!(refused.is_empty(), "{refused:?}");
        }
    }
    // This host's addresses: its loopback, and its address on a route out where it has one.
    let host = host_addresses();
    assert!(host.contains(&IpAddr::from(Ipv4Addr::LOCALHOST)), "{host:?}");
    if let Some(route) = common::routable_v4() {
        assert!(host.contains(&IpAddr::from(route)), "{host:?}");
    }
}

fn expect_line<'a>(text: &'a str, prefix: &str) -> &'a str {
    let mut lines = text.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = lines
        .next()
        .unwrap_or_else(|| panic!("missing {prefix:?} in {text}"));
    assert!(lines.next().is_none(), "one {prefix:?}");
    value
}

fn verify_helper(text: &str, inputs: &Inputs) {
    assert_eq!(expect_line(text, "confined whoami: "), "agent helper");
    let results: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("confined srv-run: "))
        .collect();
    assert_eq!(results.len(), 4);
    let identity: Value = serde_json::from_str(results[0].strip_prefix("ok ").unwrap()).unwrap();
    assert_eq!(
        identity,
        json!({"me":"agent helper","peers":[{"name":"harness driver","send":false,"answer":true}],"refused":"agent helper may not send harness driver requests"})
    );
    assert_eq!(results[1], "refused TypeError: owned-broker-error");
    assert_eq!(
        results[2],
        format!(
            "refused the program ran past its {} ms, and was ended",
            inputs.timeout_ms
        )
    );
    assert_eq!(results[3], "ok \"helper-after-abandon\"");
    let stranger: Value =
        serde_json::from_str(expect_line(text, "confined srv-run-cancel stranger: ")).unwrap();
    assert_eq!(
        stranger,
        json!({"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"\"finished\""}],"isError":false}})
    );
    let own: Value = serde_json::from_str(expect_line(text, "confined srv-run-cancel own: ")).unwrap();
    assert_eq!(
        own,
        json!({"jsonrpc":"2.0","id":2,"error":{"code":-32800,"message":"request cancelled"}})
    );
    assert_eq!(expect_line(text, "confined srv-run-abandon: "), "abandoned");
}

fn main_progress(fixture: &Fixture, inputs: &Inputs, name: &str, until: Instant) {
    let result = inputs.command(
        fixture,
        name,
        &[
            "exec",
            "owner-output",
            "/bin/testguest",
            "streams",
            "main-progress",
            "main-error",
        ],
        until,
    );
    assert_eq!(result.stdout, "main-progress\n");
    let stderr = result.stderr.strip_prefix("main-error\n").unwrap();
    assert!(stderr.is_empty() || stderr.starts_with("shards-timing "));
}

fn run(fixture: &mut Fixture, inputs: &Inputs, gate: &TcpListener, until: Instant) {
    let address = gate.local_addr().unwrap();
    let port = address.port();
    let helper = r#"const me=await shards.whoami(); const peers=await shards.peers(); let refused; try { await shards.send("harness driver","forbidden-broker-effects-nonce"); } catch(e) { refused=e.message; } return {me,peers,refused};"#;
    let timeout = format!("{}|while(true) {{}}", inputs.timeout_ms);
    let cancel = format!(
        "const t=Date.now();while(Date.now()-t<{}){{}}return \"finished\";",
        inputs.cancel_work_ms
    );
    let directives = format!(
        "ATTACH helper FOR driver\nNETWORK --egress={port} gate\nEXPOSE --harnesses=driver {port} AS egress FOR gate\nCONNECT driver WITH driver ON gate\n"
    );
    let startup_until = until.min(Instant::now().checked_add(inputs.call).unwrap());
    fixture.start_with_options(
        &[
            "confined",
            "whoami",
            "srv-run",
            helper,
            "throw new TypeError(\"owned-broker-error\");",
            "srv-run-timeout",
            &timeout,
            "srv-run-cancel",
            &cancel,
            "srv-run-abandon",
            "while(true) {}",
            "srv-run",
            "return \"helper-after-abandon\";",
        ],
        &[
            "confined",
            "hear",
            &address.to_string(),
            "whoami",
            "srv-receive",
            "0",
            "srv-run",
            "return {me:await shards.whoami(),peers:await shards.peers(),cut:\"driver-after-helper\"};",
        ],
        &directives,
        None,
        None,
        startup_until,
    );
    std::fs::copy(
        fixture.context.join("Agentfile"),
        inputs.records.join("Agentfile"),
    )
    .unwrap();
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        inputs.records.join("managed-output.ready"),
    )
    .unwrap();
    let mut streams: Vec<_> = [
        (1, Channel::Stdout),
        (1, Channel::Stderr),
        (2, Channel::Stdout),
        (2, Channel::Stderr),
    ]
    .into_iter()
    .map(|(member, channel)| {
        (
            member,
            channel,
            fixture.observe(member, channel, None, false, until),
            Capture::default(),
        )
    })
    .collect();
    for (_, channel, index, capture) in &mut streams {
        if *channel == Channel::Stderr {
            let observer = &fixture.observers[*index];
            capture.accept(observer.next(until), observer, inputs);
            assert_eq!(capture.state, StreamState::Open { offset: 0 });
        }
    }
    let (_, _, index, capture) = &mut streams[0];
    let text = capture.complete(&fixture.observers[*index], inputs, until);
    verify_helper(&text, inputs);
    inputs.write("helper-results.json",json!({"state":"ActualCanonicalHelperResultsVerified","worker_modes":["success","throw","timeout","stranger-cancel-ineffective","own-cancel","session-abandon","success-after-abandon"],"abandon_receipt":"client dropped connection only; kernel snapshot below checks visible worker absence","cancel_helper_delay_ms":300}));
    main_progress(fixture, inputs, "main-after-helper.json", until);
    release_gate(gate, inputs, until);
    let (_, _, index, capture) = &mut streams[2];
    let text = capture.complete(&fixture.observers[*index], inputs, until);
    assert_eq!(expect_line(&text, "confined whoami: "), "harness driver");
    assert_eq!(expect_line(&text, "confined srv-receive: "), "[]");
    assert_eq!(
        expect_line(&text, &format!("confined hear {address}: ")),
        "after-helper-results"
    );
    let value: Value = serde_json::from_str(expect_line(&text, "confined srv-run: ok ")).unwrap();
    assert_eq!(
        value,
        json!({"me":"harness driver","peers":[{"name":"agent helper","send":true,"answer":false}],"cut":"driver-after-helper"})
    );
    inputs.write("recipient-witness.json",json!({"state":"ActualRecipientInboxEmptyAfterDeniedHelperAttempt","held_peer":"ATTACH helper FOR driver creates real bidirectional channel; send permission only driver to helper","cut":"host released hear after helper results and main exec","limitations":"not a sender-only mutation proof; receiver independently filters unauthorized request kinds"}));
    main_progress(fixture, inputs, "main-after-recipient.json", until);
    let top = inputs.command(
        fixture,
        "guest-processes.json",
        &["top", "owner-output", "-o", "pid,ppid,uid,nlwp,stat,comm"],
        until,
    );
    let mut counts = BTreeMap::<&str, usize>::new();
    for line in top.stdout.lines().skip(1) {
        if let Some(comm) = line.split_whitespace().last() {
            *counts.entry(comm).or_default() += 1;
        }
    }
    assert_eq!(counts.get("confined-ready"), Some(&2));
    assert_eq!(counts.get("shards-server"), Some(&2));
    assert_eq!(counts.get("shards-sandbox"), Some(&2));
    assert_eq!(counts.get("shards-program").copied().unwrap_or(0), 0);
    let receipts: Vec<_> = streams
        .iter_mut()
        .map(|(_, channel, index, capture)| {
            let receipt = capture.snapshot(&fixture.observers[*index], inputs);
            if *channel == Channel::Stderr {
                assert!(capture.bytes.is_empty(), "stderr empty only at observed cut");
            }
            receipt
        })
        .collect();
    inputs.write("observation-cut.json",json!({"state":"ResultsAndNeighborMainProgressThenKernelProcessSnapshot","channels":receipts,"visible_workers":0,"visible_ready":2,"full_kernel_resource_return":"Unverified; point process snapshot and later host FD check only"}));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        inputs.records.join("managed-output.before-stop"),
    )
    .unwrap();
}

#[test]
#[ignore = "bounded actual KVM broker campaign; caller supplies explicit budgets and new evidence path"]
fn native_program_results_have_private_capture_and_owned_worker_cleanup() {
    assert!(
        !common::cannot_run_vms(),
        "assigned campaign requires a real KVM backend"
    );
    let inputs = Inputs::read();
    let until = Instant::now().checked_add(inputs.case).unwrap();
    let work_until = until.checked_sub(inputs.cleanup).unwrap();
    // The guest's egress reaches the gate at this host's address on its route out (its own
    // loopback is the guest's), so the gate listens there alone.
    let route = common::routable_v4()
        .unwrap_or_else(|| panic!("the gate needs this host's IPv4 address on a route out; there is none"));
    let gate = TcpListener::bind((route, 0)).unwrap();
    let mut fixture = Fixture::new();
    inputs.write(
        "owned-home.json",
        json!({"home":fixture.home.to_str().unwrap(),"scope":"this test's private SHARDS_HOME only"}),
    );
    inputs.write("inputs.json",json!({"case_ms":inputs.case.as_millis(),"call_ms":inputs.call.as_millis(),"cleanup_reserved_ms":inputs.cleanup.as_millis(),"timeout_ms":inputs.timeout_ms,"cancel_work_ms":inputs.cancel_work_ms,"capture_bytes_per_channel":inputs.bytes,"case_count":1,"cleanup":"same original case deadline, owner daemon stop and observer join","no_claim":"profiling, complete kernel return, durable audit and all P7 obligations remain open"}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut fixture, &inputs, &gate, work_until)
    }));
    let cleanup = fixture.shutdown(until);
    for (from, name) in [
        (fixture.home.join("daemon.log"), "daemon.log"),
        (
            fixture.record.clone().unwrap_or_default(),
            "managed-output.after-stop",
        ),
    ] {
        if from.is_file() {
            std::fs::copy(from, inputs.records.join(name)).unwrap();
        }
    }
    inputs.write("cleanup.json",json!({"body_passed":outcome.is_ok(),"owner_stop_observer_join_and_parent_fd_check":cleanup,"listener":"kept through parent FD comparison then dropped; Drop close result unobserved","uncertainty":"failed deadline/stop/join retains unknown resources; outer caller owns final process-group bound"}));
    drop(gate);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    assert!(cleanup.is_ok(), "{cleanup:?}");
}
