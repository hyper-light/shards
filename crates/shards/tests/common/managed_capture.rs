//! Shared test transport for authenticated owner-private canonical capture.
#![allow(dead_code)] // Separate integration binaries use different fixture operations.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hyper_quic::{ClientConfig, StreamId};
use sha2::{Digest as _, Sha256};
use shards_abi::run::managed::{Channel, Event as CaptureEvent, InputEnd, Record, StreamState};
use shards_remote::frame::{self, Tag};
use shards_remote::identity::{Fingerprint, Identity};
use shards_remote::managed::{self, Admitted, Reason, RunId, Scope};
use shards_remote::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};

pub(super) enum Event {
    Record(u64, Vec<u8>),
    Lost(Option<u64>),
    Failed(String),
    End,
}

impl std::fmt::Debug for Event {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Record(offset, bytes) => out
                .debug_struct("Record")
                .field("offset", offset)
                .field("bytes", &bytes.len())
                .finish(),
            Self::Lost(code) => out.debug_tuple("Lost").field(code).finish(),
            Self::Failed(error) => out.debug_tuple("Failed").field(error).finish(),
            Self::End => out.write_str("End"),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) enum ControlReceipt {
    Write { accepted: usize, pending: usize },
    Read { bytes: usize, ended: bool },
    Finish { error: Option<String> },
    WriteError(String),
}

pub(super) enum Ask {
    Connect(SocketAddr),
    Ack(u64),
    AckAndFinish(u64),
    Cancel,
    #[cfg(test)]
    RawControl {
        bytes: Vec<u8>,
        finish: bool,
        receipts: mpsc::Sender<ControlReceipt>,
    },
}

struct Client {
    config: Option<ClientConfig>,
    scope: Scope,
    conn: Option<ConnKey>,
    stream: Option<StreamId>,
    events: mpsc::Sender<Event>,
    pending: Vec<u8>,
    #[cfg(test)]
    control_receipts: Option<mpsc::Sender<ControlReceipt>>,
    inbound: Vec<u8>,
    finish: bool,
    done: bool,
    deadline: Instant,
    window: usize,
}

impl Client {
    fn fail(&mut self, message: String) {
        let _ = self.events.send(Event::Failed(message));
        self.done = true;
    }

    fn flush(&mut self, io: &mut Io<'_>) {
        let (Some(conn), Some(stream)) = (self.conn, self.stream) else {
            return;
        };
        while !self.pending.is_empty() {
            match io.write(conn, stream, &self.pending) {
                Ok(0) => return,
                Ok(n) => {
                    self.pending.drain(..n);
                    #[cfg(test)]
                    if let Some(receipts) = &self.control_receipts {
                        let _ = receipts.send(ControlReceipt::Write {
                            accepted: n,
                            pending: self.pending.len(),
                        });
                    }
                }
                Err(error) => {
                    #[cfg(test)]
                    if let Some(receipts) = &self.control_receipts {
                        let _ = receipts.send(ControlReceipt::WriteError(error.to_string()));
                    }
                    self.fail(error.to_string());
                    return;
                }
            }
        }
        if self.finish {
            let result = io.finish(conn, stream);
            #[cfg(test)]
            if let Some(receipts) = &self.control_receipts {
                let _ = receipts.send(ControlReceipt::Finish {
                    error: result.as_ref().err().map(ToString::to_string),
                });
            }
            if let Err(error) = result {
                self.fail(error.to_string());
            }
            self.finish = false;
        }
    }
}

impl Handler for Client {
    type Command = Ask;

    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> Result<(), Close> {
        self.stream = Some(io.open(conn).map_err(|_| Close {
            code: 1,
            why: "fixture stream failed",
        })?);
        self.pending.extend_from_slice(&managed::subscribe(self.scope, 0));
        self.flush(io);
        Ok(())
    }

    fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        loop {
            let (bytes, ended) = match io.read_within(conn, stream, self.window) {
                Ok(Read::Bytes(bytes)) => (bytes, false),
                Ok(Read::End(bytes)) => (bytes, true),
                Ok(Read::Blocked) => return,
                other => {
                    self.fail(format!("managed read {other:?}"));
                    return;
                }
            };
            #[cfg(test)]
            if let Some(receipts) = &self.control_receipts {
                let _ = receipts.send(ControlReceipt::Read {
                    bytes: bytes.len(),
                    ended,
                });
            }
            self.inbound.extend_from_slice(&bytes);
            loop {
                let (tag, bytes) = match frame::take(&mut self.inbound) {
                    Ok(Some(record)) => record,
                    Ok(None) => break,
                    Err(error) => {
                        self.fail(error.to_string());
                        return;
                    }
                };
                if tag != Tag::Stdout {
                    self.fail("unexpected managed data tag".into());
                    return;
                }
                let Some((offset, payload)) = bytes.split_first_chunk::<8>() else {
                    self.fail("missing managed offset".into());
                    return;
                };
                let _ = self
                    .events
                    .send(Event::Record(u64::from_be_bytes(*offset), payload.to_vec()));
            }
            if ended {
                let _ = self.events.send(Event::End);
                return;
            }
        }
    }

    fn writable(&mut self, io: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.fail("stream stopped".into());
    }
    fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, _: &str, code: Option<u64>) {
        let _ = self.events.send(Event::Lost(code));
        self.done = true;
    }

    fn command(&mut self, io: &mut Io<'_>, command: Ask) {
        match command {
            Ask::Connect(address) => match io.connect(address, self.config.take().unwrap()) {
                Ok(conn) => self.conn = Some(conn),
                Err(error) => self.fail(error.to_string()),
            },
            Ask::AckAndFinish(offset) => {
                self.pending.extend_from_slice(&managed::consumed(offset));
                self.finish = true;
                self.flush(io);
            }
            Ask::Ack(offset) => {
                self.pending.extend_from_slice(&managed::consumed(offset));
                self.flush(io);
            }
            Ask::Cancel => {
                if let Some(conn) = self.conn {
                    io.close(conn, 0, "ordinary fixture observer cancel");
                }
                self.done = true;
            }
            #[cfg(test)]
            Ask::RawControl {
                bytes,
                finish,
                receipts,
            } => {
                if bytes.len() > managed::CONTROL_BYTES
                    || self.control_receipts.is_some()
                    || !self.pending.is_empty()
                {
                    self.fail("raw control requires one bounded command after flushed subscription".into());
                    return;
                }
                self.control_receipts = Some(receipts);
                self.pending.extend_from_slice(&bytes);
                self.finish |= finish;
                self.flush(io);
            }
        }
    }

    fn done(&self) -> bool {
        self.done
    }
    fn due(&self) -> Option<Instant> {
        Some(self.deadline)
    }
    fn timeout(&mut self, _: &mut Io<'_>, _: Instant) {
        self.fail("observer deadline".into());
    }
}

/// A socket an owner observer opened, by the inode `/proc/self/fd` names it by, with the
/// descriptor number it had. A process-wide descriptor count also moves with every test
/// running beside it; an inode is this observer's socket alone.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Held {
    pub(super) fd: i32,
    pub(super) inode: u64,
}

/// This process's sockets: each inode with the descriptors that hold it.
#[cfg(target_os = "linux")]
fn sockets() -> BTreeMap<u64, Vec<i32>> {
    let mut held = BTreeMap::<u64, Vec<i32>>::new();
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        let entry = entry.unwrap();
        // A descriptor closed since the listing has no link left to read.
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Some(inode) = target
            .to_str()
            .and_then(|t| t.strip_prefix("socket:["))
            .and_then(|t| t.strip_suffix(']'))
        else {
            continue;
        };
        let fd = entry.file_name().to_str().unwrap().parse().unwrap();
        held.entry(inode.parse().unwrap()).or_default().push(fd);
    }
    held
}

/// This network namespace's IPv4 UDP sockets, each local address with its inode
/// (/proc/net/udp, whose address is the `__be32` printed as a host-order integer).
#[cfg(target_os = "linux")]
fn udp_sockets() -> Vec<(std::net::SocketAddrV4, u64)> {
    let table = std::fs::read_to_string("/proc/net/udp").unwrap();
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let (address, port) = fields.get(1)?.split_once(':')?;
            let ip = std::net::Ipv4Addr::from(u32::from_str_radix(address, 16).ok()?.to_ne_bytes());
            let port = u16::from_str_radix(port, 16).ok()?;
            Some((
                std::net::SocketAddrV4::new(ip, port),
                fields.get(9)?.parse().ok()?,
            ))
        })
        .collect()
}

/// An observer's two sockets (crates/remote/src/net.rs, bind_with_limits), each by its own
/// address, whatever other tests of the process open meanwhile: the endpoint's, and its
/// waker's, which every clone of its sender holds again. Told apart by what changed as its
/// sender was cloned, another test's sockets made in that window were taken for its waker
/// (CI's arm64 musl run 43347049202: three such).
#[cfg(target_os = "linux")]
fn observer_sockets(endpoint: SocketAddr, sender: &Sender<Ask>) -> Vec<Held> {
    let waker = sender.waker_addr().unwrap();
    let bound = |address: SocketAddr| {
        let SocketAddr::V4(at) = address else {
            panic!("an IPv4 observer socket: {address}");
        };
        let found: Vec<_> = udp_sockets()
            .into_iter()
            .filter(|(local, _)| *local == at)
            .map(|(_, inode)| inode)
            .collect();
        let [inode] = found[..] else {
            panic!("one UDP socket at the observer's {at}: {found:?}");
        };
        inode
    };
    let (endpoint, waker) = (bound(endpoint), bound(waker));
    let fds = sockets();
    let held = |inode: u64| Held {
        fd: fds[&inode][0],
        inode,
    };
    vec![held(endpoint), held(waker)]
}

/// Which of `held` are not yet returned: still behind a descriptor of this process, or
/// still a socket the kernel lists.
#[cfg(target_os = "linux")]
pub(super) fn not_returned(held: &[Held]) -> Vec<Held> {
    let ours = sockets();
    let listed: Vec<_> = udp_sockets().into_iter().map(|(_, inode)| inode).collect();
    held.iter()
        .copied()
        .filter(|socket| ours.contains_key(&socket.inode) || listed.contains(&socket.inode))
        .collect()
}

pub(super) struct Observer {
    pub(super) scope: Scope,
    pub(super) sender: Sender<Ask>,
    pub(super) events: mpsc::Receiver<Event>,
    /// Its driver's QUIC socket and its waker's.
    #[cfg(target_os = "linux")]
    pub(super) held: Vec<Held>,
    ended: mpsc::Receiver<Result<(), shards_remote::Error>>,
    outcome: OnceLock<Result<(), String>>,
    thread: Option<JoinHandle<()>>,
}

impl Observer {
    pub(super) fn start(
        owner: &Identity,
        metadata: &Metadata,
        scope: Scope,
        address: SocketAddr,
        until: Instant,
    ) -> Self {
        let window = metadata.capacity.checked_add(managed::DATA_HEAD).unwrap();
        let admitted = Admitted {
            record_bytes: metadata.capacity,
            transport_bytes: u64::try_from(window).unwrap(),
            // RFC 8446 Handshake length: uint24 plus the four-byte handshake header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: managed::DATAGRAM_BYTES,
        };
        let (event, events) = mpsc::channel();
        let client = Client {
            config: Some(managed::client_config(owner, metadata.server, admitted).unwrap()),
            scope,
            conn: None,
            stream: None,
            events: event,
            pending: Vec::new(),
            #[cfg(test)]
            control_receipts: None,
            inbound: Vec::new(),
            finish: false,
            done: false,
            deadline: until,
            window,
        };
        let (mut driver, sender) = Driver::bind_bounded(
            "127.0.0.1:0".parse().unwrap(),
            None,
            client,
            DriverLimits {
                connections: 1,
                command_slots: 1,
                queued_datagram_bytes: admitted.datagram_bytes,
            },
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        let held = observer_sockets(driver.local().unwrap(), &sender);
        let (finished, ended) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("managed-owner-observer".into())
            .spawn(move || {
                let _ = finished.send(driver.run());
            })
            .unwrap();
        sender.send(Command::Handler(Ask::Connect(address))).unwrap();
        Self {
            scope,
            sender,
            events,
            #[cfg(target_os = "linux")]
            held,
            ended,
            outcome: OnceLock::new(),
            thread: Some(thread),
        }
    }

    pub(super) fn next(&self, until: Instant) -> Event {
        match self
            .events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(event) => event,
            // The handler holds the event sender and drops with the driver once it ends.
            Err(mpsc::RecvTimeoutError::Disconnected) => match self.outcome(Duration::ZERO) {
                Some(Ok(())) => panic!("observer events ended before deadline: driver ended Ok"),
                Some(Err(error)) => panic!("observer events ended before deadline: driver {error}"),
                None => panic!("observer events ended before deadline: driver result not yet sent"),
            },
            Err(error) => panic!("observer event before deadline: {error}"),
        }
    }

    /// How the driver ended, once it has, taken from its thread within `within` and kept.
    fn outcome(&self, within: Duration) -> Option<&Result<(), String>> {
        if self.outcome.get().is_none() {
            let ended = match self.ended.recv_timeout(within) {
                Ok(result) => result.map_err(|e| format!("failed: {e}")),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err("ended without a result: its thread panicked".into())
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
            };
            let _ = self.outcome.set(ended);
        }
        self.outcome.get()
    }

    pub(super) fn exact(&self, label: &str, expected: &[u8], until: Instant) -> (u64, u64) {
        let mut bytes = Vec::new();
        let mut wire = 0u64;
        let mut state = StreamState::New;
        while bytes.len() < expected.len() {
            match self.next(until) {
                Event::Record(offset, chunk) => {
                    assert!(!chunk.is_empty(), "empty member record");
                    assert_eq!(offset, wire);
                    wire = wire.checked_add(u64::try_from(chunk.len()).unwrap()).unwrap();
                    let record = Record::decode(&chunk).unwrap();
                    assert_eq!(record.member, self.scope.member);
                    assert_eq!(record.channel, self.scope.channel);
                    state.accept(record).unwrap();
                    match record.event {
                        CaptureEvent::Open(actual) => assert_eq!(actual, label),
                        CaptureEvent::Data(payload) => {
                            bytes.extend_from_slice(payload);
                            assert!(
                                expected.starts_with(&bytes),
                                "member bytes differ at offset {offset}; lengths actual={} expected={}; controlled_fixture_bytes={bytes:02x?}",
                                bytes.len(),
                                expected.len()
                            );
                            if bytes.len() == expected.len() {
                                return (offset, wire);
                            }
                        }
                        other => panic!("unexpected capture event {other:?}"),
                    }
                    self.sender.send(Command::Handler(Ask::Ack(wire))).unwrap();
                }
                Event::Failed(error) => panic!("observer failed: {error}"),
                other => panic!("expected member record, got {other:?}"),
            }
        }
        panic!("nonempty fixture output required");
    }

    pub(super) fn refused(&self, expected: Option<Reason>, until: Instant) {
        match self.next(until) {
            Event::Lost(code) => assert_eq!(code, expected.map(|reason| u64::from(reason as u32))),
            Event::Record(offset, bytes) => {
                panic!("refused subscription received {} bytes at {offset}", bytes.len())
            }
            Event::Failed(error) => panic!("observer failed instead of peer refusal: {error}"),
            Event::End => panic!("refused subscription unexpectedly ended normally"),
        }
    }

    pub(super) fn ack_and_finish(&self, wire: u64, until: Instant) {
        self.sender
            .send(Command::Handler(Ask::AckAndFinish(wire)))
            .unwrap();
        // QUIC FIN follows the consumed frame in the same ordered stream. The service
        // processes controls before its End branch emits this typed close reason.
        self.refused(Some(Reason::Closed), until);
    }

    pub(super) fn join(&mut self, until: Instant) -> Result<(), String> {
        if self.thread.is_none() {
            return Ok(());
        }
        let Some(result) = self
            .outcome(until.saturating_duration_since(Instant::now()))
            .cloned()
        else {
            return Err("observer completion deadline".into());
        };
        let joined = self
            .thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| "observer worker panicked".to_string());
        joined.and(result)
    }
}

#[derive(Debug)]
pub(super) struct Metadata {
    pub(super) server: Fingerprint,
    pub(super) owner: Fingerprint,
    pub(super) run: String,
    pub(super) generation: u64,
    pub(super) capacity: usize,
    pub(super) endpoints: BTreeMap<(u32, u8), SocketAddr>,
}

fn fingerprint(text: &str) -> Option<Fingerprint> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (pair, byte) in text.as_bytes().as_chunks::<2>().0.iter().zip(&mut out) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

impl Metadata {
    pub(super) fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != "shards managed output 1" {
            return None;
        }
        let field = |line: &str, name| {
            line.strip_prefix(name)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let server = fingerprint(&field(lines.next()?, "server ")?)?;
        let owner = fingerprint(&field(lines.next()?, "owner ")?)?;
        let run = field(lines.next()?, "run ")?;
        let generation = field(lines.next()?, "generation ")?.parse().ok()?;
        let capacity = field(lines.next()?, "capacity ")?.parse().ok()?;
        if capacity == 0 {
            return None;
        }
        let mut endpoints = BTreeMap::new();
        for line in lines {
            let fields: Vec<_> = line.split_whitespace().collect();
            let ["member", member, "channel", channel, "address", address] = fields.as_slice() else {
                return None;
            };
            let member = member.parse().ok()?;
            let channel = channel.parse().ok()?;
            let address: SocketAddr = address.parse().ok()?;
            // 1 and 2 a member's output channels, 0 its input (plan step 5).
            if !matches!(channel, 0..=2)
                || !address.ip().is_loopback()
                || endpoints.insert((member, channel), address).is_some()
            {
                return None;
            }
        }
        Some(Self {
            server,
            owner,
            run,
            generation,
            capacity,
            endpoints,
        })
    }

    pub(super) fn scope(&self, member: u32, channel: Channel) -> Scope {
        Scope {
            run: RunId(Sha256::digest(self.run.as_bytes()).into()),
            generation: self.generation,
            member,
            channel,
        }
    }
}

/// What an owner writing a member's input heard of its endpoint.
#[derive(Debug)]
pub(super) enum InputHeard {
    Said(managed::input::Said),
    End,
    Lost(Option<u64>),
    Failed(String),
}

pub(super) enum InputAsk {
    Connect(SocketAddr),
    Send(Vec<u8>),
    /// Closes its connection, as an owner that goes does: its member's input stays open.
    Close,
}

/// An owner of a member's input, as an orchestrator's adapter is: it subscribes, then sends
/// what it is asked to, and tells what the endpoint says.
struct InputClient {
    config: Option<ClientConfig>,
    scope: managed::input::Scope,
    offset: u64,
    stream: Option<(ConnKey, StreamId)>,
    pending: Vec<u8>,
    inbound: Vec<u8>,
    heard: mpsc::Sender<InputHeard>,
    deadline: Instant,
    done: bool,
}

impl InputClient {
    fn flush(&mut self, io: &mut Io<'_>) {
        let Some((conn, stream)) = self.stream else {
            return;
        };
        while !self.pending.is_empty() {
            match io.write(conn, stream, &self.pending) {
                Ok(0) => return,
                Ok(n) => drop(self.pending.drain(..n)),
                Err(error) => {
                    let _ = self.heard.send(InputHeard::Failed(error.to_string()));
                    self.done = true;
                    return;
                }
            }
        }
    }
}

impl Handler for InputClient {
    type Command = InputAsk;
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> Result<(), Close> {
        let stream = io.open(conn).map_err(|_| Close {
            code: 1,
            why: "fixture input stream failed",
        })?;
        self.stream = Some((conn, stream));
        let mut first = managed::input::subscribe(self.scope, self.offset).to_vec();
        first.append(&mut self.pending);
        self.pending = first;
        self.flush(io);
        Ok(())
    }
    fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        loop {
            let (bytes, ended) = match io.read(conn, stream) {
                Read::Bytes(bytes) => (bytes, false),
                Read::End(bytes) => (bytes, true),
                Read::Blocked => return,
                Read::Reset => {
                    let _ = self.heard.send(InputHeard::Failed("input stream reset".into()));
                    self.done = true;
                    return;
                }
            };
            self.inbound.extend_from_slice(&bytes);
            loop {
                match managed::input::said(&self.inbound) {
                    Ok(Some((said, n))) => {
                        let _ = self.heard.send(InputHeard::Said(said));
                        self.inbound.drain(..n);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = self.heard.send(InputHeard::Failed(error.to_string()));
                        self.done = true;
                        return;
                    }
                }
            }
            if ended {
                let _ = self.heard.send(InputHeard::End);
                return;
            }
        }
    }
    fn writable(&mut self, io: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {
        let _ = self.heard.send(InputHeard::Failed("input stream stopped".into()));
        self.done = true;
    }
    fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, _: &str, code: Option<u64>) {
        let _ = self.heard.send(InputHeard::Lost(code));
        self.done = true;
    }
    fn command(&mut self, io: &mut Io<'_>, ask: InputAsk) {
        match ask {
            InputAsk::Connect(address) => {
                if let Some(Err(error)) = self.config.take().map(|config| io.connect(address, config)) {
                    let _ = self.heard.send(InputHeard::Failed(error.to_string()));
                    self.done = true;
                }
            }
            InputAsk::Send(bytes) => {
                self.pending.extend_from_slice(&bytes);
                self.flush(io);
            }
            InputAsk::Close => {
                if let Some((conn, _)) = self.stream {
                    io.close(conn, 0, "fixture input owner gone");
                }
                self.done = true;
            }
        }
    }
    fn done(&self) -> bool {
        self.done
    }
    fn due(&self) -> Option<Instant> {
        Some(self.deadline)
    }
    fn timeout(&mut self, _: &mut Io<'_>, _: Instant) {
        let _ = self
            .heard
            .send(InputHeard::Failed("input writer deadline".into()));
        self.done = true;
    }
}

/// A member's input, written by its owner over its endpoint: one record in flight, each
/// sent once the endpoint says the last was taken.
pub(super) struct InputWriter {
    pub(super) sender: Sender<InputAsk>,
    pub(super) heard: mpsc::Receiver<InputHeard>,
    thread: Option<JoinHandle<Result<(), shards_remote::Error>>>,
    /// Where the next record goes and how long it may be, as last said.
    pub(super) at: u64,
    room: usize,
}

impl InputWriter {
    /// Subscribes to `member`'s input at `address`, from offset `offset`, and waits to be
    /// told where its first record goes.
    pub(super) fn start(
        owner: &Identity,
        metadata: &Metadata,
        member: u32,
        address: SocketAddr,
        offset: u64,
        until: Instant,
    ) -> Result<Self, InputHeard> {
        let admitted = Admitted {
            record_bytes: shards_abi::run::CHUNK,
            transport_bytes: u64::try_from(managed::input::TAKEN_BYTES).unwrap(),
            // RFC 8446 Handshake length: uint24 plus the four-byte handshake header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: managed::DATAGRAM_BYTES,
        };
        let (heard, hearing) = mpsc::channel();
        let client = InputClient {
            config: Some(managed::input::client_config(owner, metadata.server, admitted).unwrap()),
            scope: managed::input::Scope {
                run: RunId(Sha256::digest(metadata.run.as_bytes()).into()),
                generation: metadata.generation,
                member,
            },
            offset,
            stream: None,
            pending: Vec::new(),
            inbound: Vec::new(),
            heard,
            deadline: until,
            done: false,
        };
        let (mut driver, sender) = Driver::bind_bounded(
            "127.0.0.1:0".parse().unwrap(),
            None,
            client,
            DriverLimits {
                connections: 1,
                command_slots: 4,
                queued_datagram_bytes: admitted.datagram_bytes,
            },
        )
        .unwrap();
        let thread = std::thread::Builder::new()
            .name("fixture-input".into())
            .spawn(move || driver.run())
            .unwrap();
        sender.send(Command::Handler(InputAsk::Connect(address))).unwrap();
        let mut writer = Self {
            sender,
            heard: hearing,
            thread: Some(thread),
            at: offset,
            room: 0,
        };
        writer.taken(until)?;
        Ok(writer)
    }

    /// Waits for the endpoint to say where the next record goes.
    fn taken(&mut self, until: Instant) -> Result<u64, InputHeard> {
        match self
            .heard
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(InputHeard::Said(managed::input::Said::Taken { accepted, room })) => {
                self.at = accepted;
                self.room = room as usize;
                Ok(accepted)
            }
            Ok(other) => Err(other),
            Err(_) => Err(InputHeard::Failed(
                "no taken within the fixture's patience".into(),
            )),
        }
    }

    /// Writes `bytes` to the member's stdin, a record at a time, each within the room it was
    /// told, waiting for each to be taken; the offset after them.
    pub(super) fn write_all(&mut self, bytes: &[u8], until: Instant) -> Result<u64, InputHeard> {
        for part in bytes.chunks(self.room.max(1)) {
            let mut frame = Vec::new();
            managed::input::record(self.at, part, &mut frame).unwrap();
            self.sender.send(Command::Handler(InputAsk::Send(frame))).unwrap();
            let before = self.at;
            let at = self.taken(until)?;
            // A record is taken whole, at its offset.
            if Some(at) != before.checked_add(part.len() as u64) {
                return Err(InputHeard::Failed(format!(
                    "a record of {} bytes at {before} taken to {at}",
                    part.len()
                )));
            }
        }
        Ok(self.at)
    }

    /// Ends the member's stdin (`errno` 0) or aborts it, and waits for its end.
    pub(super) fn end(&mut self, errno: i32, until: Instant) -> Result<(u64, InputEnd), InputHeard> {
        self.sender
            .send(Command::Handler(InputAsk::Send(
                managed::input::end(self.at, errno).to_vec(),
            )))
            .unwrap();
        match self
            .heard
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(InputHeard::Said(managed::input::Said::Ended { written, end })) => Ok((written, end)),
            Ok(other) => Err(other),
            Err(_) => Err(InputHeard::Failed("no end within the fixture's patience".into())),
        }
    }
}

impl Drop for InputWriter {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Handler(InputAsk::Close));
        self.sender.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
