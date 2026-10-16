//! A daemon and its clients over QUIC on this machine's loopback (D180): a client joins by
//! an invitation, then runs commands, their output and status crossing whole; a client not
//! enrolled, and a join with another secret, are refused in the daemon's words. And what a
//! stream's ends may do to it: a daemon end it with its last frames at once, a command leave
//! its stdin unread, a client send none, leave the stream, or go, none of which holds up,
//! keeps or reaches another command.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use hyper_quic::StreamId;
use shards_remote::Error;
use shards_remote::client::{self, Controls, Remote};
use shards_remote::frame::{self, Request, Tag};
use shards_remote::identity::Identity;
use shards_remote::invite::{self, Invitation};
use shards_remote::net::{self, Close, ConnKey, Driver, Handler, Io, Read as StreamRead};
use shards_remote::server::{self, Host, Running, Stopper};
use shards_remote::tls;
use shards_testdir::TempDir;

/// How long anything here may take before the test fails rather than waits.
const DEADLINE: Duration = Duration::from_secs(60);

/// What the machine saw of a command, known by its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Started(String),
    /// Its stdin was let go: the daemon holds it no more.
    InputClosed(String),
    HungUp(String),
    Exited(String, i32),
    Resized(String, u16, u16),
    Signalled(String, u32),
    /// The daemon let go of its stream's state, the means to hang it up with it.
    Released(String),
}

/// A machine whose clients and invitations are kept in memory, running each command as a
/// process of its own, and saying what it saw of each.
struct Machine {
    clients: Mutex<Vec<(String, Vec<u8>)>>,
    invitations: Mutex<Vec<Invitation>>,
    seen: mpsc::Sender<Seen>,
}

/// A command's stdin, which says when it is let go.
struct Told {
    stdin: std::process::ChildStdin,
    what: String,
    seen: mpsc::Sender<Seen>,
}

impl std::io::Write for Told {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stdin.flush()
    }
}

impl Drop for Told {
    fn drop(&mut self) {
        let _ = self.seen.send(Seen::InputClosed(self.what.clone()));
    }
}

/// What a command's hangup holds, which says when the daemon lets go of it.
struct Held(mpsc::Sender<Seen>, String);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.send(Seen::Released(self.1.clone()));
    }
}

impl Host for Machine {
    fn enrolled(&self, spki: &[u8]) -> Option<String> {
        let clients = self.clients.lock().unwrap();
        clients
            .iter()
            .find(|(_, key)| key == spki)
            .map(|(name, _)| name.clone())
    }

    fn join(&self, name: &str, proof: &[u8], exported: &[u8; 32], spki: &[u8]) -> Result<(), Error> {
        let mut invitations = self.invitations.lock().unwrap();
        let at = invitations
            .iter()
            .position(|i| i.name == name && invite::verify(&i.secret, exported, proof))
            .ok_or_else(|| Error::new("no"))?;
        invitations.remove(at);
        self.clients
            .lock()
            .unwrap()
            .push((name.to_string(), spki.to_vec()));
        Ok(())
    }

    fn start(&self, _client: &str, request: Request) -> Result<Running, Error> {
        let args: Vec<String> = request
            .args
            .iter()
            .map(|a| String::from_utf8(a.clone()).unwrap())
            .collect();
        let what = args.join(" ");
        let mut child = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(e.to_string()))?;
        let stdin = Told {
            stdin: child.stdin.take().unwrap(),
            what: what.clone(),
            seen: self.seen.clone(),
        };
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let pid = child.id();
        let _ = self.seen.send(Seen::Started(what.clone()));
        let (exited, hung) = (self.seen.clone(), self.seen.clone());
        let hung_what = what.clone();
        let held = Held(self.seen.clone(), what.clone());
        let (resized, signalled) = (self.seen.clone(), self.seen.clone());
        let (resized_what, signalled_what) = (what.clone(), what.clone());
        Ok(Running {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            wait: Box::new(move || {
                let status = child.wait().map(|s| s.code().unwrap_or(128)).unwrap_or(1);
                let _ = exited.send(Seen::Exited(what, status));
                status
            }),
            hangup: Box::new(move || {
                let _ = &held;
                let _ = hung.send(Seen::HungUp(hung_what.clone()));
                let _ = Command::new("kill").args(["-HUP", &pid.to_string()]).status();
            }),
            resize: Some(Box::new(move |rows, cols| {
                let _ = resized.send(Seen::Resized(resized_what.clone(), rows, cols));
            })),
            signal: Box::new(move |linux| {
                let _ = signalled.send(Seen::Signalled(signalled_what.clone(), linux));
            }),
        })
    }

    fn invited(&self) -> usize {
        self.invitations.lock().unwrap().len()
    }
}

/// What a machine saw, kept as it comes, so that a wait for one thing loses none of the rest.
struct Watch {
    rx: mpsc::Receiver<Seen>,
    seen: Vec<Seen>,
}

impl Watch {
    /// Waits until the machine has seen `what`.
    fn saw(&mut self, what: &Seen) {
        let until = Instant::now() + DEADLINE;
        while !self.seen.contains(what) {
            let left = until.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(seen) => self.seen.push(seen),
                Err(_) => panic!("the machine never saw {what:?}; it saw {:?}", self.seen),
            }
        }
    }

    /// What the machine has seen by now.
    fn now(&mut self) -> &[Seen] {
        while let Ok(seen) = self.rx.try_recv() {
            self.seen.push(seen);
        }
        &self.seen
    }
}

/// A daemon serving a [`Machine`] on loopback, on a thread of its own, stopped as it drops,
/// as it does when a test fails: the test fails, rather than waits.
struct Daemon {
    remote: Remote,
    stopper: Stopper,
    seen: Watch,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.stopper.stop();
    }
}

fn daemon(
    identity: &'static Identity,
    clients: Vec<(String, Vec<u8>)>,
    invitations: Vec<Invitation>,
) -> Daemon {
    let (seen, rx) = mpsc::channel();
    let machine = Machine {
        clients: Mutex::new(clients),
        invitations: Mutex::new(invitations),
        seen,
    };
    let (tx, bound) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = server::serve(
            identity,
            "127.0.0.1:0".parse().unwrap(),
            machine,
            |at, stopper| {
                let _ = tx.send((at, stopper));
            },
        );
    });
    let (address, stopper) = bound.recv_timeout(DEADLINE).unwrap();
    Daemon {
        remote: Remote {
            address,
            daemon: identity.fingerprint(),
        },
        stopper,
        seen: Watch { rx, seen: Vec::new() },
    }
}

fn leak(identity: Identity) -> &'static Identity {
    Box::leak(Box::new(identity))
}

/// What a writer the command's output goes to holds, once the command has ended.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

type Ran = (Result<i32, String>, Vec<u8>, Vec<u8>);

/// Runs `args` on `remote` as `identity`, `stdin` sent where given, on a thread of its own:
/// what [`Pending::get`] gives once it has ended.
fn run(
    identity: &'static Identity,
    remote: Remote,
    args: &[&str],
    stdin: Option<Box<dyn Read + Send>>,
) -> Pending<Ran> {
    run_steered(identity, remote, args, stdin, None)
}

/// [`run`], what steers the command given to `steered`.
fn run_steered(
    identity: &'static Identity,
    remote: Remote,
    args: &[&str],
    stdin: Option<Box<dyn Read + Send>>,
    steered: Option<mpsc::Sender<Controls>>,
) -> Pending<Ran> {
    let args: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (out, err) = (Captured::default(), Captured::default());
        let request = Request {
            args,
            env: Vec::new(),
            terminal: None,
        };
        let steer = |controls| {
            if let Some(steered) = steered {
                let _ = steered.send(controls);
            }
        };
        let status = client::exec(identity, remote, request, stdin, out.clone(), err.clone(), steer).unwrap();
        let _ = tx.send((status, out.bytes(), err.bytes()));
    });
    Pending(rx)
}

/// What a thread of a test will give.
struct Pending<T>(mpsc::Receiver<T>);

impl<T> Pending<T> {
    fn get(self) -> T {
        self.0
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("not ended within {DEADLINE:?}"))
    }
}

fn input(bytes: &[u8]) -> Option<Box<dyn Read + Send>> {
    Some(Box::new(std::io::Cursor::new(bytes.to_vec())))
}

#[test]
fn a_client_joins_then_runs_commands_whose_output_crosses_whole() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let stranger = leak(Identity::generate().unwrap());
    // The invitation is made before the daemon serves: the address is filled in after.
    let pending = Invitation::new("laptop", "127.0.0.1:1".parse().unwrap(), daemon_id.fingerprint()).unwrap();
    let d = daemon(daemon_id, Vec::new(), vec![pending.clone()]);
    let remote = d.remote;
    let invitation = Invitation {
        address: remote.address,
        ..pending
    };

    // Not enrolled: refused.
    let (status, _, _) = run(laptop, remote, &["true"], None).get();
    assert_eq!(status, Err(server::NOT_ENROLLED.to_string()));
    // Another secret: refused, in the same words as any join that fails.
    let forged = Invitation {
        secret: [9; 32],
        ..invitation.clone()
    };
    assert_eq!(
        client::join(stranger, &forged).unwrap(),
        Err(server::NOT_INVITED.to_string())
    );
    // The invitation: enrolled, once.
    assert_eq!(client::join(laptop, &invitation).unwrap(), Ok(()));
    assert_eq!(
        client::join(stranger, &invitation).unwrap(),
        Err(server::NOT_INVITED.to_string())
    );

    let (status, out, err) = run(
        laptop,
        remote,
        &["sh", "-c", "echo out; echo err >&2; exit 3"],
        None,
    )
    .get();
    assert_eq!(status, Ok(3));
    assert_eq!(out, b"out\n");
    assert_eq!(err, b"err\n");
    let (status, out, _) = run(laptop, remote, &["cat"], input(b"hello over quic")).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out, b"hello over quic");
    // Eight MiB of stdin, more than its pipe and the daemon's room for it: it crosses whole.
    let (status, out, _) = run(laptop, remote, &["cat"], input(&vec![7; 8 << 20])).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out.len(), 8 << 20);
    assert!(out.iter().all(|b| *b == 7));
    // Eight MiB, more than a stream's window: it crosses whole.
    let (status, out, _) = run(laptop, remote, &["sh", "-c", "head -c 8388608 /dev/zero"], None).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out.len(), 8 << 20);
    assert!(out.iter().all(|b| *b == 0));
    // Still not enrolled.
    let (status, _, _) = run(stranger, remote, &["true"], None).get();
    assert_eq!(status, Err(server::NOT_ENROLLED.to_string()));
}

/// A daemon that answers each request with `answer`, written at once with its stream's end.
struct Answers {
    answer: Vec<u8>,
    inbound: HashMap<StreamId, Vec<u8>>,
}

impl Handler for Answers {
    type Command = ();

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.readable(io, conn, stream);
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let buffer = self.inbound.entry(stream).or_default();
        loop {
            match io.read(conn, stream) {
                StreamRead::Bytes(bytes) => buffer.extend_from_slice(&bytes),
                StreamRead::End(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    break;
                }
                StreamRead::Blocked | StreamRead::Reset => break,
            }
        }
        if let Ok(Some((Tag::Exec, _))) = frame::take(buffer) {
            assert_eq!(io.write(conn, stream, &self.answer).unwrap(), self.answer.len());
            io.finish(conn, stream).unwrap();
        }
    }

    fn writable(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn lost(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _why: &str, _code: Option<u64>) {}
    fn command(&mut self, _io: &mut Io<'_>, _command: ()) {}

    fn done(&self) -> bool {
        false
    }
}

/// A driver's sender that stops it as it drops.
struct Stops(net::Sender<()>);

impl Drop for Stops {
    fn drop(&mut self) {
        let _ = self.0.send(net::Command::Stop);
    }
}

/// A command's every frame and its stream's end, come at once, are all taken: its output
/// written whole and its status given, as the end is judged after all the stream carried.
#[test]
fn a_streams_last_frames_and_its_end_come_at_once_and_are_all_taken() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut answer = Vec::new();
    frame::put(&mut answer, Tag::Stdout, b"out\n").unwrap();
    frame::put(&mut answer, Tag::Stderr, b"err\n").unwrap();
    frame::put(&mut answer, Tag::Exit, &3i32.to_be_bytes()).unwrap();
    let (tx, bound) = mpsc::channel();
    std::thread::spawn(move || {
        let handler = Answers {
            answer,
            inbound: HashMap::new(),
        };
        let config = tls::server(daemon_id).unwrap();
        let (mut driver, sender) =
            Driver::bind("127.0.0.1:0".parse().unwrap(), Some(config), handler).unwrap();
        let _ = tx.send((driver.local().unwrap(), sender));
        let _ = driver.run();
    });
    let (address, sender) = bound.recv_timeout(DEADLINE).unwrap();
    let _stops = Stops(sender);
    let remote = Remote {
        address,
        daemon: daemon_id.fingerprint(),
    };
    for _ in 0..20 {
        let (status, out, err) = run(laptop, remote, &["anything"], None).get();
        assert_eq!(status, Ok(3));
        assert_eq!(out, b"out\n");
        assert_eq!(err, b"err\n");
    }
}

/// A daemon that reads none of a request's stdin (STOP_SENDING at once), and answers each
/// request when the test says.
struct Unread {
    asked: mpsc::Sender<(ConnKey, StreamId)>,
    inbound: HashMap<StreamId, Vec<u8>>,
}

impl Handler for Unread {
    type Command = (ConnKey, StreamId);

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.readable(io, conn, stream);
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let buffer = self.inbound.entry(stream).or_default();
        while let StreamRead::Bytes(bytes) = io.read(conn, stream) {
            buffer.extend_from_slice(&bytes);
        }
        if let Ok(Some((Tag::Exec, _))) = frame::take(buffer) {
            io.stop(conn, stream);
            let _ = self.asked.send((conn, stream));
        }
    }

    fn writable(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn lost(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _why: &str, _code: Option<u64>) {}

    fn command(&mut self, io: &mut Io<'_>, (conn, stream): (ConnKey, StreamId)) {
        let mut answer = Vec::new();
        frame::put(&mut answer, Tag::Exit, &5i32.to_be_bytes()).unwrap();
        assert_eq!(io.write(conn, stream, &answer).unwrap(), answer.len());
        io.finish(conn, stream).unwrap();
    }

    fn done(&self) -> bool {
        false
    }
}

/// A daemon that reads no more of a client's stdin ends none of its command: the client
/// sends the rest nowhere, and waits for the answer, which comes after.
#[test]
fn a_client_asked_to_send_no_more_stdin_waits_for_its_answer() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let (tx, bound) = mpsc::channel();
    let (asked_tx, asked) = mpsc::channel();
    std::thread::spawn(move || {
        let handler = Unread {
            asked: asked_tx,
            inbound: HashMap::new(),
        };
        let config = tls::server(daemon_id).unwrap();
        let (mut driver, sender) =
            Driver::bind("127.0.0.1:0".parse().unwrap(), Some(config), handler).unwrap();
        let _ = tx.send((driver.local().unwrap(), sender));
        let _ = driver.run();
    });
    let (address, sender) = bound.recv_timeout(DEADLINE).unwrap();
    let remote = Remote {
        address,
        daemon: daemon_id.fingerprint(),
    };
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let later = run(laptop, remote, &["anything"], endless);
    let (conn, stream) = asked.recv_timeout(DEADLINE).unwrap();
    // Its client sends stdin on meanwhile, to a stream stopped.
    std::thread::sleep(Duration::from_millis(200));
    sender.send(net::Command::Handler((conn, stream))).unwrap();
    let (status, _, _) = later.get();
    assert_eq!(status, Ok(5));
    let _ = sender.send(net::Command::Stop);
}

/// What steers a command reaches it on a stream of its own, though its stdin waits for room
/// it does not get: its terminal's size, and the signals its client receives.
#[test]
fn what_steers_a_command_passes_its_stdin_waiting_for_room() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let dir = TempDir::new("remote-steer").unwrap();
    let go = dir.path().join("go");
    let waits = format!("while [ ! -e '{}' ]; do sleep 0.01; done", go.display());
    let what = format!("sh -c {waits}");
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let (steered, controls) = mpsc::channel();
    let later = run_steered(laptop, d.remote, &["sh", "-c", &waits], endless, Some(steered));
    let controls = controls.recv_timeout(DEADLINE).unwrap();
    d.seen.saw(&Seen::Started(what.clone()));
    // Long enough for its pipe, and the daemon's room for its stdin, to fill.
    std::thread::sleep(Duration::from_millis(300));
    controls.resize(40, 120).unwrap();
    controls.signal(10).unwrap();
    d.seen.saw(&Seen::Resized(what.clone(), 40, 120));
    d.seen.saw(&Seen::Signalled(what, 10));
    std::fs::write(&go, b"").unwrap();
    let (status, _, _) = later.get();
    assert_eq!(status, Ok(0));
}

/// A command's stdin ends as its client says: at once, for a client that sends none; and a
/// command that ends without reading it all ends its client all the same, the rest unsent.
#[test]
fn a_commands_stdin_ends_as_its_client_says_or_with_the_command() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let (status, out, _) = run(laptop, d.remote, &["cat"], None).get();
    assert_eq!(status, Ok(0));
    assert!(out.is_empty());
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let (status, _, _) = run(laptop, d.remote, &["true"], endless).get();
    assert_eq!(status, Ok(0));
}

/// A command that reads none of the stdin its client sends holds up no other command: the
/// daemon reads its stream no further, as its stdin has no room, and serves the rest.
#[test]
fn a_command_that_reads_no_stdin_holds_up_no_other() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let dir = TempDir::new("remote-held").unwrap();
    let go = dir.path().join("go");
    let waits = format!("while [ ! -e '{}' ]; do sleep 0.01; done", go.display());
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let held = run(laptop, d.remote, &["sh", "-c", &waits], endless);
    d.seen.saw(&Seen::Started(format!("sh -c {waits}")));
    // Long enough for its pipe, and the daemon's room for its stdin, to fill.
    std::thread::sleep(Duration::from_millis(300));
    let (status, out, _) = run(laptop, d.remote, &["echo", "hi"], None).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out, b"hi\n");
    std::fs::write(&go, b"").unwrap();
    let (status, _, _) = held.get();
    assert_eq!(status, Ok(0));
}

/// What a raw client hears.
#[derive(Debug, PartialEq, Eq)]
enum Heard {
    Connected,
    Frame(StreamId, Tag, Vec<u8>),
    /// Its connection ended, its peer's error code with it where its peer closed it.
    Lost(Option<u64>),
}

/// What a test asks of a raw client.
enum Ask {
    Connect(SocketAddr),
    /// Opens a stream, writes these bytes to it, finishing it if asked, and tells its id.
    Open(Vec<u8>, bool, mpsc::Sender<StreamId>),
    Reset(StreamId),
    Stop(StreamId),
    Close,
}

/// A client that sends the bytes a test gives it, keeps its connection as long as the test
/// does, and tells the test each frame it is sent.
struct Raw {
    config: Option<hyper_quic::ClientConfig>,
    heard: mpsc::Sender<Heard>,
    /// The streams the daemon reads no more of (STOP_SENDING).
    stops: mpsc::Sender<StreamId>,
    conn: Option<ConnKey>,
    inbound: HashMap<StreamId, Vec<u8>>,
    done: bool,
}

impl Handler for Raw {
    type Command = Ask;

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        let _ = self.heard.send(Heard::Connected);
        Ok(())
    }

    fn opened(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let buffer = self.inbound.entry(stream).or_default();
        loop {
            match io.read(conn, stream) {
                StreamRead::Bytes(bytes) => buffer.extend_from_slice(&bytes),
                StreamRead::End(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    break;
                }
                StreamRead::Blocked | StreamRead::Reset => break,
            }
        }
        while let Ok(Some((tag, payload))) = frame::take(buffer) {
            let _ = self.heard.send(Heard::Frame(stream, tag, payload));
        }
    }

    fn writable(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}

    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, stream: StreamId) {
        let _ = self.stops.send(stream);
    }

    fn lost(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _why: &str, code: Option<u64>) {
        let _ = self.heard.send(Heard::Lost(code));
        self.done = true;
    }

    fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
        match ask {
            Ask::Connect(to) => {
                let config = self.config.take().unwrap();
                self.conn = Some(io.connect(to, config).unwrap());
            }
            Ask::Open(bytes, finish, told) => {
                let conn = self.conn.unwrap();
                let stream = io.open(conn).unwrap();
                assert_eq!(io.write(conn, stream, &bytes).unwrap(), bytes.len());
                if finish {
                    io.finish(conn, stream).unwrap();
                }
                let _ = told.send(stream);
            }
            Ask::Reset(stream) => io.reset(self.conn.unwrap(), stream),
            Ask::Stop(stream) => io.stop(self.conn.unwrap(), stream),
            Ask::Close => {
                io.close(self.conn.unwrap(), 0, "");
                self.done = true;
            }
        }
    }

    fn done(&self) -> bool {
        self.done
    }
}

/// A raw client of `remote`, as `identity`, connected, on a thread of its own.
struct RawClient {
    ask: net::Sender<Ask>,
    heard: mpsc::Receiver<Heard>,
    stops: mpsc::Receiver<StreamId>,
    driver: Option<std::thread::JoinHandle<()>>,
}

impl RawClient {
    fn new(identity: &'static Identity, remote: Remote) -> RawClient {
        let (heard_tx, heard) = mpsc::channel();
        let (stops_tx, stops) = mpsc::channel();
        let (tx, made) = mpsc::channel();
        let driver = std::thread::spawn(move || {
            let raw = Raw {
                config: Some(tls::client(identity, remote.daemon).unwrap()),
                heard: heard_tx,
                stops: stops_tx,
                conn: None,
                inbound: HashMap::new(),
                done: false,
            };
            let (mut driver, sender) = Driver::bind("127.0.0.1:0".parse().unwrap(), None, raw).unwrap();
            let _ = tx.send(sender);
            let _ = driver.run();
        });
        let ask = made.recv_timeout(DEADLINE).unwrap();
        ask.send(net::Command::Handler(Ask::Connect(remote.address)))
            .unwrap();
        let client = RawClient {
            ask,
            heard,
            stops,
            driver: Some(driver),
        };
        assert_eq!(client.next(), Heard::Connected);
        client
    }

    fn next(&self) -> Heard {
        self.heard
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("heard nothing within {DEADLINE:?}"))
    }

    /// Opens a stream with `bytes` on it; its id.
    fn open(&self, bytes: Vec<u8>) -> StreamId {
        self.opens(bytes, false)
    }

    /// Opens a stream with `bytes` on it, finished if `finish`; its id.
    fn opens(&self, bytes: Vec<u8>, finish: bool) -> StreamId {
        let (told, id) = mpsc::channel();
        self.ask
            .send(net::Command::Handler(Ask::Open(bytes, finish, told)))
            .unwrap();
        id.recv_timeout(DEADLINE).unwrap()
    }

    fn ask(&self, ask: Ask) {
        self.ask.send(net::Command::Handler(ask)).unwrap();
    }

    /// Closes its connection, and waits for its driver to have sent the close.
    fn close(mut self) {
        self.ask(Ask::Close);
        if let Some(driver) = self.driver.take() {
            driver.join().unwrap();
        }
    }

    /// Waits until the daemon has asked it to send no more of `stream`.
    fn stopped(&self, stream: StreamId) {
        let until = Instant::now() + DEADLINE;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.stops.recv_timeout(left) {
                Ok(stopped) if stopped == stream => return,
                Ok(_) => {}
                Err(_) => panic!("{stream} not stopped within {DEADLINE:?}"),
            }
        }
    }
}

impl Drop for RawClient {
    fn drop(&mut self) {
        let _ = self.ask.send(net::Command::Stop);
    }
}

/// Waits until `path` is there.
fn until_there(path: &std::path::Path) {
    let until = Instant::now() + DEADLINE;
    while !path.exists() {
        assert!(
            Instant::now() < until,
            "{} not there within {DEADLINE:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `args`' request frame, and none of its stdin's end.
fn exec(args: &[&str]) -> Vec<u8> {
    let request = Request {
        args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
        env: Vec::new(),
        terminal: None,
    };
    let mut out = Vec::new();
    frame::put(&mut out, Tag::Exec, &request.encode().unwrap()).unwrap();
    out
}

/// A command's state goes with it: once its exit is sent, the daemon lets go of its stdin,
/// though its client neither ended that nor its connection, which runs another command.
#[test]
fn a_commands_state_goes_with_it_while_its_connection_stays() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let first = raw.open(exec(&["true"]));
    assert_eq!(
        raw.next(),
        Heard::Frame(first, Tag::Exit, 0i32.to_be_bytes().to_vec())
    );
    // What it still sends goes unread: it is asked to send no more.
    raw.stopped(first);
    d.seen.saw(&Seen::InputClosed("true".into()));
    d.seen.saw(&Seen::Released("true".into()));
    let second = raw.open(exec(&["echo", "again"]));
    assert_eq!(raw.next(), Heard::Frame(second, Tag::Stdout, b"again\n".to_vec()));
    assert_eq!(
        raw.next(),
        Heard::Frame(second, Tag::Exit, 0i32.to_be_bytes().to_vec())
    );
    d.seen.saw(&Seen::InputClosed("echo again".into()));
    d.seen.saw(&Seen::Released("echo again".into()));
    raw.ask(Ask::Close);
}

/// A client that finishes its stream with the last of its stdin ends the command's input
/// there: the command reads all of it, then its end, and its exit crosses back.
#[test]
fn a_client_that_finishes_its_stream_ends_its_commands_input() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let mut bytes = exec(&["cat"]);
    frame::put(&mut bytes, Tag::Stdin, b"the last of it").unwrap();
    let stream = raw.opens(bytes, true);
    assert_eq!(
        raw.next(),
        Heard::Frame(stream, Tag::Stdout, b"the last of it".to_vec())
    );
    assert_eq!(
        raw.next(),
        Heard::Frame(stream, Tag::Exit, 0i32.to_be_bytes().to_vec())
    );
}

/// A client the machine has not enrolled is refused by its frame's header: neither a
/// command's bytes nor more than a join's are waited for.
#[test]
fn a_client_not_enrolled_is_refused_by_its_frames_header() {
    let daemon_id = leak(Identity::generate().unwrap());
    let stranger = leak(Identity::generate().unwrap());
    let d = daemon(daemon_id, Vec::new(), vec![invitation(daemon_id)]);
    let raw = RawClient::new(stranger, d.remote);
    let header = |tag: Tag, len: usize| {
        let mut out = vec![tag as u8];
        out.extend_from_slice(&u32::try_from(len).unwrap().to_be_bytes());
        out
    };
    let command = raw.open(header(Tag::Exec, frame::MAX_FRAME));
    assert_eq!(
        raw.next(),
        Heard::Frame(command, Tag::Refused, server::NOT_ENROLLED.as_bytes().to_vec())
    );
    let join = raw.open(header(Tag::Join, invite::JOIN_MAX + 1));
    assert_eq!(
        raw.next(),
        Heard::Frame(join, Tag::Refused, server::NOT_INVITED.as_bytes().to_vec())
    );
    let controls = raw.open(header(Tag::Controls, 8));
    assert_eq!(
        raw.next(),
        Heard::Frame(
            controls,
            Tag::Refused,
            b"a frame out of place (Controls)".to_vec()
        )
    );
}

/// A control stream steers a command of its own connection's, or none: one that names none
/// is refused.
#[test]
fn a_control_stream_for_no_command_is_refused() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let mut named = Vec::new();
    frame::put(&mut named, Tag::Controls, &96u64.to_be_bytes()).unwrap();
    let stream = raw.open(named);
    assert_eq!(
        raw.next(),
        Heard::Frame(
            stream,
            Tag::Refused,
            b"no command of this connection's to steer".to_vec()
        )
    );
}

/// An invitation of the daemon `of`, for no one in particular.
fn invitation(of: &Identity) -> Invitation {
    Invitation::new("someone", "127.0.0.1:1".parse().unwrap(), of.fingerprint()).unwrap()
}

/// A client's refusal at its handshake's end, the daemon's code.
fn unenrolled() -> Heard {
    Heard::Lost(Some(u64::from(server::UNENROLLED)))
}

/// The machine takes as many clients it has not enrolled at once as it holds invitations,
/// each of which enrolls one: none, while it holds none.
#[test]
fn a_machine_takes_as_many_strangers_as_it_holds_invitations() {
    let daemon_id = leak(Identity::generate().unwrap());
    let strangers: Vec<&'static Identity> = (0..4).map(|_| leak(Identity::generate().unwrap())).collect();
    let d = daemon(daemon_id, Vec::new(), vec![invitation(daemon_id)]);
    let first = RawClient::new(strangers[0], d.remote);
    // Refused at its handshake's end, not let go at its time's.
    let start = Instant::now();
    let second = RawClient::new(strangers[1], d.remote);
    assert_eq!(second.next(), unenrolled());
    assert!(start.elapsed() < tls::IDLE, "{:?}", start.elapsed());
    // The first is served: a join too long, refused by its header.
    let mut join = vec![Tag::Join as u8];
    join.extend_from_slice(&u32::try_from(invite::JOIN_MAX + 1).unwrap().to_be_bytes());
    let stream = first.open(join.clone());
    assert_eq!(
        first.next(),
        Heard::Frame(stream, Tag::Refused, server::NOT_INVITED.as_bytes().to_vec())
    );
    // Gone, its place is another's.
    first.close();
    let third = RawClient::new(strangers[2], d.remote);
    let stream = third.open(join);
    assert_eq!(
        third.next(),
        Heard::Frame(stream, Tag::Refused, server::NOT_INVITED.as_bytes().to_vec())
    );
    // A machine that holds no invitation takes none.
    let none = daemon(daemon_id, Vec::new(), Vec::new());
    let start = Instant::now();
    let fourth = RawClient::new(strangers[3], none.remote);
    assert_eq!(fourth.next(), unenrolled());
    assert!(start.elapsed() < tls::IDLE, "{:?}", start.elapsed());
}

/// A client the machine has not enrolled has as long to join as a client waits for its
/// daemon, the idle bound: its connection kept alive, the daemon closes it then.
#[test]
fn a_stranger_has_the_idle_bound_to_join() {
    let daemon_id = leak(Identity::generate().unwrap());
    let stranger = leak(Identity::generate().unwrap());
    let d = daemon(daemon_id, Vec::new(), vec![invitation(daemon_id)]);
    let start = Instant::now();
    let raw = RawClient::new(stranger, d.remote);
    assert_eq!(raw.next(), unenrolled());
    let took = start.elapsed();
    assert!(took >= tls::IDLE, "closed after {took:?}, before {:?}", tls::IDLE);
}

/// A client the machine enrolled keeps its connection past the idle bound, as long as its
/// command runs.
#[test]
fn an_enrolled_clients_command_runs_past_the_idle_bound() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let past = (tls::IDLE.as_secs() + 1).to_string();
    let (status, _, _) = run(laptop, d.remote, &["sleep", &past], None).get();
    assert_eq!(status, Ok(0));
}

/// A command whose client leaves its stream, resetting it or reading no more of it, is hung
/// up, and its state let go.
#[test]
fn a_command_whose_client_leaves_its_stream_is_hung_up() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let reset = raw.open(exec(&["sleep", "31"]));
    d.seen.saw(&Seen::Started("sleep 31".into()));
    raw.ask(Ask::Reset(reset));
    d.seen.saw(&Seen::HungUp("sleep 31".into()));
    d.seen.saw(&Seen::InputClosed("sleep 31".into()));
    d.seen.saw(&Seen::Released("sleep 31".into()));
    let stopped = raw.open(exec(&["sleep", "32"]));
    d.seen.saw(&Seen::Started("sleep 32".into()));
    raw.ask(Ask::Stop(stopped));
    d.seen.saw(&Seen::HungUp("sleep 32".into()));
    d.seen.saw(&Seen::InputClosed("sleep 32".into()));
    d.seen.saw(&Seen::Released("sleep 32".into()));
}

/// What a command whose client has gone still writes, and its exit, reach no other client:
/// not the one whose connection came after, whatever QUIC streams the two had.
#[test]
fn what_a_command_of_a_connection_gone_writes_reaches_no_other() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let dir = TempDir::new("remote-gone").unwrap();
    let (go, done, ready) = (
        dir.path().join("go"),
        dir.path().join("done"),
        dir.path().join("ready"),
    );
    // It goes on through the hangup its client's going brings, once it has said it will.
    let stale = format!(
        "trap '' HUP; : > '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; echo stale",
        ready.display(),
        go.display()
    );
    {
        let raw = RawClient::new(laptop, d.remote);
        raw.open(exec(&["sh", "-c", &stale]));
        until_there(&ready);
        raw.ask(Ask::Close);
    }
    d.seen.saw(&Seen::HungUp(format!("sh -c {stale}")));
    // The connection gone, and drained: three probe timeouts on loopback.
    std::thread::sleep(Duration::from_millis(500));
    let fresh = format!(
        "while [ ! -e '{}' ]; do sleep 0.01; done; echo fresh; exit 7",
        done.display()
    );
    let later = run(laptop, d.remote, &["sh", "-c", &fresh], None);
    d.seen.saw(&Seen::Started(format!("sh -c {fresh}")));
    std::fs::write(&go, b"").unwrap();
    d.seen.saw(&Seen::Exited(format!("sh -c {stale}"), 0));
    // What it wrote, and its exit, reach the daemon's driver meanwhile.
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(&done, b"").unwrap();
    let (status, out, _) = later.get();
    assert_eq!(status, Ok(7), "{:?}", d.seen.now());
    assert_eq!(String::from_utf8_lossy(&out), "fresh\n");
}
