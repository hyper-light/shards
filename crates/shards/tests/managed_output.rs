//! Supplied AGENT/HARNESS output through the daemon's private owner endpoint.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hyper_quic::{ClientConfig, StreamId};
use sha2::{Digest as _, Sha256};
use shards_abi::run::managed::{Channel, Event as CaptureEvent, Record, StreamState};
use shards_remote::frame::{self, Tag};
use shards_remote::identity::{Fingerprint, Identity};
use shards_remote::managed::{self, Admitted, Reason, RunId, Scope};
use shards_remote::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};

// Test patience, matching the neighboring real-VM build fixtures; never runtime policy.
const WITHIN: Duration = Duration::from_secs(120);

enum Event {
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

enum Ask {
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

struct Observer {
    scope: Scope,
    sender: Sender<Ask>,
    events: mpsc::Receiver<Event>,
    ended: mpsc::Receiver<Result<(), shards_remote::Error>>,
    thread: Option<JoinHandle<()>>,
}

impl Observer {
    fn start(
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

    fn next(&self, until: Instant) -> Event {
        self.events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("observer event before deadline: {error}"))
    }

    fn exact(&self, label: &str, expected: &[u8], until: Instant) -> (u64, u64) {
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

    fn refused(&self, expected: Option<Reason>, until: Instant) {
        match self.next(until) {
            Event::Lost(code) => assert_eq!(code, expected.map(|reason| u64::from(reason as u32))),
            Event::Record(offset, bytes) => {
                panic!("refused subscription received {} bytes at {offset}", bytes.len())
            }
            Event::Failed(error) => panic!("observer failed instead of peer refusal: {error}"),
            Event::End => panic!("refused subscription unexpectedly ended normally"),
        }
    }

    fn ack_and_finish(&self, wire: u64, until: Instant) {
        self.sender
            .send(Command::Handler(Ask::AckAndFinish(wire)))
            .unwrap();
        // QUIC FIN follows the consumed frame in the same ordered stream. The service
        // processes controls before its End branch emits this typed close reason.
        self.refused(Some(Reason::Closed), until);
    }

    fn join(&mut self, until: Instant) -> Result<(), String> {
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
struct Metadata {
    server: Fingerprint,
    owner: Fingerprint,
    run: String,
    generation: u64,
    capacity: usize,
    endpoints: BTreeMap<(u32, u8), SocketAddr>,
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
    fn parse(text: &str) -> Option<Self> {
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

    fn scope(&self, member: u32, channel: Channel) -> Scope {
        Scope {
            run: RunId(Sha256::digest(self.run.as_bytes()).into()),
            generation: self.generation,
            member,
            channel,
        }
    }
}

fn payload(member: &str, channel: Channel) -> Vec<u8> {
    let tail: &[u8] = match channel {
        Channel::Stdout => b"\0stdout\n\xff\x1b[2Jout-tail",
        Channel::Stderr => b"\0stderr\n\x80\xfeerr-tail",
    };
    [member.as_bytes(), tail].concat()
}

struct Fixture {
    home: common::TempDir,
    context: common::TempDir,
    owner: Identity,
    metadata: Option<Metadata>,
    record: Option<PathBuf>,
    observers: Vec<Observer>,
    observed: BTreeMap<(u32, u8), (u64, u64)>,
    initial_fds: Option<usize>,
}

impl Fixture {
    fn new() -> Self {
        let home = common::TempDir::new("owner-output");
        let owner = Identity::load_or_create(&home.join("identity")).unwrap();
        Self {
            home,
            context: common::TempDir::new("owner-context"),
            owner,
            metadata: None,
            record: None,
            observers: Vec::new(),
            observed: BTreeMap::new(),
            initial_fds: None,
        }
    }

    fn command(&self, args: &[&str], within: Duration) -> common::Run {
        common::run_shards_env(
            &[],
            args,
            &[
                ("SHARDS_HOME", self.home.as_os_str()),
                ("SHARDS_KERNEL", common::kernel().as_os_str()),
                ("SHARDS_INIT", common::guest_init().as_os_str()),
            ],
            within,
        )
    }

    fn start(&mut self) {
        self.start_with_options(
            &["managed-capture", "agent"],
            &["managed-capture", "harness"],
            "",
            Instant::now() + WITHIN,
        );
    }

    fn start_with_options(&mut self, agent: &[&str], harness: &[&str], directives: &str, until: Instant) {
        let (image, _) = common::served();
        std::fs::create_dir(self.context.join("files")).unwrap();
        std::fs::write(self.context.join("files/asset"), b"files-only").unwrap();
        std::fs::create_dir(self.context.join("agent")).unwrap();
        std::fs::copy(common::test_guest(), self.context.join("agent/member")).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            self.context.join("agent/member"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(
            self.context.join("agent/agent.json"),
            serde_json::to_vec(&serde_json::json!({"entrypoint":"member","options":agent})).unwrap(),
        )
        .unwrap();
        let binary = std::fs::read(common::test_guest()).unwrap();
        let manifest =
            serde_json::to_vec(&serde_json::json!({"entrypoint":"member","options":harness})).unwrap();
        std::fs::write(
            self.context.join("harness.tar"),
            common::tar(&[
                ("bin", 0o755, 0, None),
                ("bin/member", 0o755, 0, Some(&binary)),
                ("harness.json", 0o644, 0, Some(&manifest)),
            ]),
        )
        .unwrap();
        std::fs::write(self.context.join("Agentfile"), format!("FROM {image}\nAGENT assets FROM ./files\nAGENT helper FROM ./agent\nHARNESS driver FROM ./harness.tar\n{directives}")).unwrap();
        let built = self.command(
            &[
                "build",
                "image",
                "-t",
                "owner-output:1",
                self.context.to_str().unwrap(),
            ],
            until.saturating_duration_since(Instant::now()),
        );
        assert_eq!(built.status, Some(0), "{built}");
        let started = self.command(
            &[
                "run",
                "vm",
                "-d",
                "--name",
                "owner-output",
                "owner-output:1",
                "sleep",
            ],
            until.saturating_duration_since(Instant::now()),
        );
        assert_eq!(started.status, Some(0), "{started}");
        let id = started.stdout.trim();
        assert!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "unexpected detached run identity"
        );
        let record = self.home.join("containers").join(id).join("managed-output");
        let metadata = Metadata::parse(&std::fs::read_to_string(&record).unwrap())
            .unwrap_or_else(|| panic!("published owner metadata"));
        assert_eq!(metadata.run, id);
        assert_eq!(metadata.server, self.owner.fingerprint());
        assert_eq!(metadata.owner, self.owner.fingerprint());
        assert_eq!(
            metadata.endpoints.keys().copied().collect::<Vec<_>>(),
            [(1, 1), (1, 2), (2, 1), (2, 2)],
            "files-only member retains its declaration hole"
        );
        self.record = Some(record);
        self.metadata = Some(metadata);
        self.initial_fds = Some(std::fs::read_dir("/proc/self/fd").unwrap().count());
    }

    fn observe(
        &mut self,
        member: u32,
        channel: Channel,
        offered: Option<Scope>,
        outsider: bool,
        until: Instant,
    ) -> usize {
        let metadata = self.metadata.as_ref().unwrap();
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner subscribe member={member} channel={} outsider={outsider}",
            channel as u8
        );
        let stranger = outsider.then(|| Identity::generate().unwrap());
        let observer = Observer::start(
            stranger.as_ref().unwrap_or(&self.owner),
            metadata,
            offered.unwrap_or_else(|| metadata.scope(member, channel)),
            metadata.endpoints[&(member, channel as u8)],
            until,
        );
        self.observers.push(observer);
        self.observers.len() - 1
    }

    fn main_progress(&self, within: Duration) {
        let _ = writeln!(std::io::stdout(), "managed-owner main exec");
        let progress = self.command(
            &[
                "exec",
                "owner-output",
                "/bin/testguest",
                "streams",
                "main-progress",
                "main-error",
            ],
            within,
        );
        assert_eq!(progress.status, Some(0), "{progress}");
        assert_eq!(progress.stdout, "main-progress\n", "{progress}");
        let stderr = progress
            .stderr
            .strip_prefix("main-error\n")
            .unwrap_or_else(|| panic!("independent main stderr"));
        assert!(
            stderr.is_empty() || stderr.starts_with("shards-timing "),
            "unexpected main diagnostic"
        );
    }

    fn consume(&mut self, member: u32, label: &str, channel: Channel) {
        let observer = self.observe(member, channel, None, false, Instant::now() + WITHIN);
        let expected = payload(label, channel);
        let (_, wire) =
            self.observers[observer].exact(declared_label(member), &expected, Instant::now() + WITHIN);
        self.observers[observer].ack_and_finish(wire, Instant::now() + WITHIN);
        self.observed.insert((member, channel as u8), (wire, wire));
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner consumed member={member} channel={} bytes={}",
            channel as u8,
            expected.len()
        );
    }

    fn shutdown(&mut self, until: Instant) -> Result<(), String> {
        let _ = writeln!(std::io::stdout(), "managed-owner owner stop");
        let mut errors = Vec::new();
        for observer in &self.observers {
            observer.sender.request_stop();
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.command(
                &["stop", "daemon"],
                until.saturating_duration_since(Instant::now()),
            )
        })) {
            Ok(stopped) if stopped.status != Some(0) => errors.push(format!("owner stop: {stopped}")),
            Err(_) => errors.push("owner-stop CLI helper panicked".into()),
            _ => {}
        }
        for observer in &mut self.observers {
            if let Err(error) = observer.join(until) {
                errors.push(error);
            }
        }
        self.observers.clear();
        if let Some(before) = self.initial_fds {
            match std::fs::read_dir("/proc/self/fd") {
                Ok(entries) => {
                    let after = entries.count();
                    let _ = writeln!(
                        std::io::stdout(),
                        "managed-owner observer_fds before={before} after={after}"
                    );
                    if after != before {
                        errors.push(format!("observer FD return {before} -> {after}"));
                    }
                }
                Err(error) => errors.push(format!("observer FD observation: {error}")),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn terminal(&self, member: u32, channel: Channel) -> (u64, u64) {
        let record = std::fs::read_to_string(self.record.as_ref().unwrap()).unwrap();
        let prefix = format!("member {member} channel {} state ", channel as u8);
        let terminal = record
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("terminal channel metadata"));
        let fields: Vec<_> = terminal.split_whitespace().collect();
        let [
            "aborted",
            "accepted",
            accepted,
            "consumed",
            consumed,
            "tail",
            "unknown",
        ] = fields.as_slice()
        else {
            panic!("unexpected terminal metadata");
        };
        let cursors = (accepted.parse().unwrap(), consumed.parse().unwrap());
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner terminal member={member} channel={} accepted={} consumed={} tail=unknown",
            channel as u8,
            cursors.0,
            cursors.1
        );
        cursors
    }
}

fn declared_label(member: u32) -> &'static str {
    match member {
        1 => "agent helper",
        2 => "harness driver",
        _ => panic!("fixture has no runnable member {member}"),
    }
}

fn with_fixture(test: impl FnOnce(&mut Fixture)) -> Option<Fixture> {
    if common::cannot_run_vms() {
        return None;
    }
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.start();
        test(&mut fixture);
    }));
    let stopped = fixture.shutdown(Instant::now() + WITHIN);
    if let Err(error) = result {
        let _ = writeln!(std::io::stderr(), "owner cleanup: {stopped:?}");
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    Some(fixture)
}

#[test]
fn supplied_owner_output_keeps_bytes_ack_cancel_and_neighbor_progress_independent() {
    let Some(fixture) = with_fixture(|fixture| {
        let held = fixture.observe(1, Channel::Stdout, None, false, Instant::now() + WITHIN);
        let (start, end) = fixture.observers[held].exact(
            declared_label(1),
            &payload("agent", Channel::Stdout),
            Instant::now() + WITHIN,
        );
        fixture.observed.insert((1, Channel::Stdout as u8), (end, start));
        fixture.consume(1, "agent", Channel::Stderr);
        fixture.main_progress(WITHIN);
        fixture.observers[held]
            .sender
            .send(Command::Handler(Ask::Cancel))
            .unwrap();
        fixture.observers[held].join(Instant::now() + WITHIN).unwrap();
        let _ = writeln!(std::io::stdout(), "managed-owner local cancellation complete");
        fixture.main_progress(WITHIN);
        // The sibling receives its first output credit only after observer loss.
        fixture.consume(2, "harness", Channel::Stdout);
        fixture.consume(2, "harness", Channel::Stderr);
    }) else {
        return;
    };
    // Cancellation preserves a record awaiting ACK instead of manufacturing
    // consumption or holding owner teardown behind its stalled observer.
    for (&(member, channel), &expected) in &fixture.observed {
        assert_eq!(
            fixture.terminal(member, Channel::try_from(channel).unwrap()),
            expected
        );
    }
}

#[test]
fn private_owner_output_refuses_wrong_owner_stale_generation_and_cross_member_scope() {
    let Some(fixture) = with_fixture(|fixture| {
        let outsider = fixture.observe(1, Channel::Stdout, None, true, Instant::now() + WITHIN);
        // The current public Driver erases the typed TLS cause. A peer close without
        // an application code is observed; local errors/deadlines/data are failures.
        fixture.observers[outsider].refused(None, Instant::now() + WITHIN);
        let mut stale = fixture.metadata.as_ref().unwrap().scope(1, Channel::Stderr);
        stale.generation = stale.generation.checked_add(1).unwrap();
        let observer = fixture.observe(1, Channel::Stderr, Some(stale), false, Instant::now() + WITHIN);
        fixture.observers[observer].refused(Some(Reason::Stale), Instant::now() + WITHIN);
        let other = fixture.metadata.as_ref().unwrap().scope(1, Channel::Stdout);
        let observer = fixture.observe(2, Channel::Stdout, Some(other), false, Instant::now() + WITHIN);
        fixture.observers[observer].refused(Some(Reason::Scope), Instant::now() + WITHIN);
        fixture.consume(2, "harness", Channel::Stderr);
        fixture.main_progress(WITHIN);
    }) else {
        return;
    };
    for (member, channel) in [(1, Channel::Stdout), (1, Channel::Stderr), (2, Channel::Stdout)] {
        assert_eq!(fixture.terminal(member, channel), (0, 0));
    }
    assert_eq!(
        fixture.terminal(2, Channel::Stderr),
        fixture.observed[&(2, Channel::Stderr as u8)]
    );
}

#[test]
fn metadata_requires_unique_scoped_loopback_endpoints() {
    let key = "01".repeat(32);
    let header =
        format!("shards managed output 1\nserver {key}\nowner {key}\nrun abc\ngeneration 9\ncapacity 64\n");
    let endpoint = "member 2 channel 1 address 127.0.0.1:12345\n";
    let metadata = Metadata::parse(&format!("{header}{endpoint}")).unwrap();
    assert_eq!(metadata.scope(2, Channel::Stdout).member, 2);
    assert!(Metadata::parse(&format!("{header}{endpoint}{endpoint}")).is_none());
    assert!(Metadata::parse(&format!("{header}member 2 channel 1 address 192.0.2.1:12345\n")).is_none());
    assert!(Metadata::parse(&format!("{header}member 2 channel 3 address 127.0.0.1:12345\n")).is_none());
}

#[path = "../../../docs/research/measurements/managed-output-cost/fixture.rs"]
mod cost;
