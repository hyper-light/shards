//! Shared test transport for authenticated owner-private canonical capture.
#![allow(dead_code)] // Separate integration binaries use different fixture operations.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

use hyper_quic::{ClientConfig, StreamId};
use sha2::{Digest as _, Sha256};
use shards_abi::run::managed::{Channel, Event as CaptureEvent, Record, StreamState};
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

pub(super) enum Ask {
    Connect(SocketAddr),
    Ack(u64),
    AckAndFinish(u64),
    Cancel,
}

struct Client {
    config: Option<ClientConfig>,
    scope: Scope,
    conn: Option<ConnKey>,
    stream: Option<StreamId>,
    events: mpsc::Sender<Event>,
    pending: Vec<u8>,
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
                }
                Err(error) => {
                    self.fail(error.to_string());
                    return;
                }
            }
        }
        if self.finish {
            if let Err(error) = io.finish(conn, stream) {
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

pub(super) struct Observer {
    pub(super) scope: Scope,
    pub(super) sender: Sender<Ask>,
    pub(super) events: mpsc::Receiver<Event>,
    ended: mpsc::Receiver<Result<(), shards_remote::Error>>,
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
        let patience = until.saturating_duration_since(Instant::now());
        let window = metadata.capacity.checked_add(managed::DATA_HEAD).unwrap();
        let admitted = Admitted {
            record_bytes: metadata.capacity,
            transport_bytes: u64::try_from(window).unwrap(),
            // RFC 8446 Handshake length: uint24 plus the four-byte handshake header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: managed::DATAGRAM_BYTES,
            patience,
        };
        let (event, events) = mpsc::channel();
        let client = Client {
            config: Some(managed::client_config(owner, metadata.server, admitted).unwrap()),
            scope,
            conn: None,
            stream: None,
            events: event,
            pending: Vec::new(),
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
                handshake_patience: patience,
            },
        )
        .unwrap();
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
            ended,
            thread: Some(thread),
        }
    }

    pub(super) fn next(&self, until: Instant) -> Event {
        self.events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("observer event before deadline: {error}"))
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
        let result = match self
            .ended
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("observer completion disconnected".into()),
            Err(mpsc::RecvTimeoutError::Timeout) => return Err("observer completion deadline".into()),
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
            if !matches!(channel, 1 | 2)
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
